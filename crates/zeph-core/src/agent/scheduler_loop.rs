// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use zeph_llm::provider::LlmProvider;

use super::Agent;
use super::error;
use super::shutdown_signal;
use super::tool_execution;

impl<C: crate::channel::Channel> Agent<C> {
    /// Cancel all agents referenced in `cancel_actions`.
    ///
    /// Returns `Some(status)` if a `Done` action is encountered, `None` otherwise.
    pub(super) fn cancel_agents_from_actions(
        &mut self,
        cancel_actions: Vec<zeph_orchestration::SchedulerAction>,
    ) -> Option<zeph_orchestration::GraphStatus> {
        use zeph_orchestration::SchedulerAction;
        for action in cancel_actions {
            match action {
                SchedulerAction::Cancel { agent_handle_id } => {
                    if let Some(mgr) = self.orchestration.subagent_manager.as_mut() {
                        let _ = mgr.cancel(&agent_handle_id).inspect_err(|e| {
                            tracing::trace!(error = %e, "cancel: agent already gone");
                        });
                    }
                }
                SchedulerAction::Done { status } => return Some(status),
                SchedulerAction::Spawn { .. }
                | SchedulerAction::RunInline { .. }
                | SchedulerAction::Verify { .. }
                | SchedulerAction::VerifyPredicate { .. } => {}
            }
        }
        None
    }

    /// Handle a `SchedulerAction::Spawn` — attempt to spawn a sub-agent for the given task.
    ///
    /// Returns `(spawn_success, concurrency_fail, done_status)`.
    /// `done_status` is `Some` when spawn failure forces the scheduler to emit a `Done` action.
    pub(super) async fn handle_scheduler_spawn_action(
        &mut self,
        scheduler: &mut zeph_orchestration::DagScheduler,
        task_id: zeph_orchestration::TaskId,
        agent_def_name: String,
        prompt: String,
        spawn_counter: &mut usize,
        task_count: usize,
    ) -> (bool, bool, Option<zeph_orchestration::GraphStatus>) {
        let task_title = scheduler
            .graph()
            .tasks
            .get(task_id.index())
            .map_or("unknown", |t| t.title.as_str());

        let provider = self.provider.clone();
        let tool_executor = Arc::clone(&self.tool_executor);
        let skills = self.filtered_skills_for(&agent_def_name);
        let cfg = self.orchestration.subagent_config.clone();
        let event_tx = scheduler.event_sender();

        let spawn_ctx = self.build_spawn_context(&cfg);

        let mgr = self
            .orchestration
            .subagent_manager
            .as_mut()
            .expect("subagent_manager checked above");

        let on_done = {
            use zeph_orchestration::{TaskEvent, TaskOutcome};
            move |handle_id: String, result: Result<String, zeph_subagent::SubAgentError>| {
                let outcome = match &result {
                    Ok(output) => TaskOutcome::Completed {
                        output: output.clone(),
                        artifacts: vec![],
                    },
                    Err(e) => TaskOutcome::Failed {
                        error: e.to_string(),
                    },
                };
                let tx = event_tx;
                tokio::spawn(async move {
                    if let Err(e) = tx
                        .send(TaskEvent {
                            task_id,
                            agent_handle_id: handle_id,
                            outcome,
                        })
                        .await
                    {
                        tracing::warn!(
                            error = %e,
                            "failed to send TaskEvent: scheduler may have been dropped"
                        );
                    }
                });
            }
        };

        match mgr.spawn_for_task(
            &agent_def_name,
            &prompt,
            provider,
            tool_executor,
            skills,
            &cfg,
            spawn_ctx,
            on_done,
        ) {
            Ok(handle_id) => {
                *spawn_counter += 1;
                let _ = self
                    .channel
                    .send_status(&format!(
                        "Executing task {spawn_counter}/{task_count}: {task_title}..."
                    ))
                    .await;
                scheduler.record_spawn(task_id, handle_id, agent_def_name);
                (true, false, None)
            }
            Err(e) => {
                tracing::error!(error = %e, %task_id, "spawn_for_task failed");
                let concurrency_fail =
                    matches!(e, zeph_subagent::SubAgentError::ConcurrencyLimit { .. });
                let extra = scheduler.record_spawn_failure(task_id, &e);
                let done_status = self.cancel_agents_from_actions(extra);
                (false, concurrency_fail, done_status)
            }
        }
    }

    /// Execute a `RunInline` scheduler action: run the task synchronously in the current agent.
    ///
    /// Sends a status update, registers the spawn with the scheduler, runs the inline tool
    /// loop (or cancels on token fire), and posts the completion event back to the scheduler.
    pub(super) async fn handle_run_inline_action(
        &mut self,
        scheduler: &mut zeph_orchestration::DagScheduler,
        task_id: zeph_orchestration::TaskId,
        prompt: String,
        spawn_counter: usize,
        task_count: usize,
        cancel_token: &CancellationToken,
    ) {
        let task_title = scheduler
            .graph()
            .tasks
            .get(task_id.index())
            .map_or("unknown", |t| t.title.as_str());
        let _ = self
            .channel
            .send_status(&format!(
                "Executing task {spawn_counter}/{task_count} (inline): {task_title}..."
            ))
            .await;

        let handle_id = format!("__inline_{task_id}__");
        scheduler.record_spawn(task_id, handle_id.clone(), "__main__".to_string());

        let event_tx = scheduler.event_sender();
        let max_iter = self.tool_orchestrator.max_iterations;
        let outcome = tokio::select! {
            result = self.run_inline_tool_loop(&prompt, max_iter) => {
                match result {
                    Ok(output) => zeph_orchestration::TaskOutcome::Completed {
                        output,
                        artifacts: vec![],
                    },
                    Err(e) => zeph_orchestration::TaskOutcome::Failed {
                        error: e.to_string(),
                    },
                }
            }
            () = cancel_token.cancelled() => {
                zeph_orchestration::TaskOutcome::Failed {
                    error: "canceled".to_string(),
                }
            }
        };
        let event = zeph_orchestration::TaskEvent {
            task_id,
            agent_handle_id: handle_id,
            outcome,
        };
        if let Err(e) = event_tx.send(event).await {
            tracing::warn!(%task_id, error = %e, "inline task event send failed");
        }
    }

    // too_many_lines: sequential scheduler event loop with 4 tokio::select! branches
    // (cancel token, scheduler tick, channel recv with /plan cancel + channel-close paths,
    // shutdown signal) — each branch requires distinct cancel/fail/ignore semantics that
    // cannot be split without introducing shared mutable state across async boundaries.
    #[allow(clippy::too_many_lines)]
    /// Drive the [`DagScheduler`] tick loop until it emits `SchedulerAction::Done`.
    ///
    /// Each iteration yields at `wait_event()`, during which `channel.recv()` is polled
    /// concurrently via `tokio::select!`. If the user sends `/plan cancel`, all running
    /// sub-agent tasks are aborted and the loop exits with [`GraphStatus::Canceled`].
    /// If the channel is closed (`Ok(None)`), all running sub-agent tasks are aborted
    /// and the loop exits with [`GraphStatus::Failed`].
    /// Other messages received during execution are queued in `message_queue` and
    /// processed after the plan completes.
    ///
    /// # Known limitations
    ///
    /// `RunInline` tasks block the tick loop for their entire duration — `/plan cancel`
    /// cannot interrupt an in-progress inline LLM call and will only be delivered on the
    /// next iteration after the call completes.
    pub(super) async fn run_scheduler_loop(
        &mut self,
        scheduler: &mut zeph_orchestration::DagScheduler,
        task_count: usize,
        cancel_token: CancellationToken,
    ) -> Result<zeph_orchestration::GraphStatus, error::AgentError> {
        use zeph_orchestration::{PlanVerifier, SchedulerAction};

        let mut spawn_counter: usize = 0;

        let mut denied_secrets: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();

        let mut plan_verifier: Option<PlanVerifier<zeph_llm::any::AnyProvider>> = None;
        let mut stdin_closed = false;
        // In-flight dedupe for VerifyPredicate actions (S9): prevents double-charging
        // the LLM when tick() re-emits the same task before the previous eval completes.
        // Reset on process restart — restart-safety is provided by predicate_outcome.is_none().
        let mut in_flight_predicate_evals: std::collections::HashSet<zeph_orchestration::TaskId> =
            std::collections::HashSet::new();

        let final_status = 'tick: loop {
            let actions = scheduler.tick();

            let mut any_spawn_success = false;
            let mut any_concurrency_failure = false;

            for action in actions {
                match action {
                    SchedulerAction::Spawn {
                        task_id,
                        agent_def_name,
                        prompt,
                    } => {
                        let (success, fail, done) = self
                            .handle_scheduler_spawn_action(
                                scheduler,
                                task_id,
                                agent_def_name,
                                prompt,
                                &mut spawn_counter,
                                task_count,
                            )
                            .await;
                        any_spawn_success |= success;
                        any_concurrency_failure |= fail;
                        if let Some(s) = done {
                            break 'tick s;
                        }
                    }
                    SchedulerAction::Cancel { agent_handle_id } => {
                        if let Some(mgr) = self.orchestration.subagent_manager.as_mut() {
                            let _ = mgr.cancel(&agent_handle_id).inspect_err(|e| {
                                tracing::trace!(error = %e, "cancel: agent already gone");
                            });
                        }
                    }
                    SchedulerAction::RunInline { task_id, prompt } => {
                        spawn_counter += 1;
                        self.handle_run_inline_action(
                            scheduler,
                            task_id,
                            prompt,
                            spawn_counter,
                            task_count,
                            &cancel_token,
                        )
                        .await;
                    }
                    SchedulerAction::Done { status } => {
                        break 'tick status;
                    }
                    SchedulerAction::VerifyPredicate {
                        task_id,
                        predicate,
                        output,
                    } => {
                        // Dedupe: skip if an evaluation for this task is already in flight.
                        if in_flight_predicate_evals.contains(&task_id) {
                            continue;
                        }
                        in_flight_predicate_evals.insert(task_id);

                        // Resolve predicate provider: verify_provider fallback > primary.
                        // Full named-provider resolution requires provider_pool lookup which is
                        // not yet exposed here; use verify_provider as the preferred alternate.
                        let predicate_provider = self
                            .orchestration
                            .verify_provider
                            .as_ref()
                            .unwrap_or(&self.provider)
                            .clone();

                        let prior_reason = scheduler
                            .predicate_failure_reason(task_id)
                            .map(str::to_string);
                        let max_tasks = self.orchestration.orchestration_config.max_tasks as usize;

                        let timeout_secs = self
                            .orchestration
                            .orchestration_config
                            .predicate_timeout_secs;
                        let sanitizer = zeph_sanitizer::ContentSanitizer::new(
                            &zeph_sanitizer::ContentIsolationConfig::default(),
                        );
                        let evaluator = zeph_orchestration::PredicateEvaluator::new(
                            predicate_provider,
                            sanitizer,
                            timeout_secs,
                        );
                        let outcome = evaluator
                            .evaluate(&predicate, &output, prior_reason.as_deref())
                            .await;

                        tracing::debug!(
                            task_id = %task_id,
                            passed = outcome.passed,
                            confidence = outcome.confidence,
                            "predicate evaluation result"
                        );

                        in_flight_predicate_evals.remove(&task_id);

                        if let Err(e) =
                            scheduler.record_predicate_outcome(task_id, outcome, max_tasks)
                        {
                            tracing::warn!(
                                error = %e,
                                task_id = %task_id,
                                "record_predicate_outcome failed (fail-open)"
                            );
                        }
                    }
                    SchedulerAction::Verify { task_id, output } => {
                        let verify_provider = self
                            .orchestration
                            .verify_provider
                            .as_ref()
                            .unwrap_or(&self.provider)
                            .clone();
                        let threshold = self
                            .orchestration
                            .orchestration_config
                            .completeness_threshold;
                        let sanitizer = self.security.sanitizer.clone();

                        let verifier = plan_verifier
                            .get_or_insert_with(|| PlanVerifier::new(verify_provider, sanitizer));

                        let task = scheduler.graph().tasks.get(task_id.index()).cloned();

                        if let Some(task) = task {
                            let result = verifier.verify(&task, &output).await;
                            tracing::debug!(
                                task_id = %task_id,
                                complete = result.complete,
                                confidence = result.confidence,
                                gaps = result.gaps.len(),
                                "per-task verification result"
                            );

                            let should_replan = !result.complete
                                && result.confidence < f64::from(threshold)
                                && result.gaps.iter().any(|g| {
                                    matches!(
                                        g.severity,
                                        zeph_orchestration::GapSeverity::Critical
                                            | zeph_orchestration::GapSeverity::Important
                                    )
                                });

                            if should_replan {
                                let max_tasks_u32 =
                                    self.orchestration.orchestration_config.max_tasks;
                                let max_tasks = max_tasks_u32 as usize;
                                match verifier
                                    .replan(&task, &result.gaps, scheduler.graph(), max_tasks_u32)
                                    .await
                                {
                                    Ok(new_tasks) if !new_tasks.is_empty() => {
                                        if let Err(e) =
                                            scheduler.inject_tasks(task_id, new_tasks, max_tasks)
                                        {
                                            tracing::warn!(
                                                error = %e,
                                                task_id = %task_id,
                                                "per-task replan inject_tasks failed (fail-open)"
                                            );
                                        }
                                    }
                                    Ok(_) => {}
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            task_id = %task_id,
                                            "per-task replan failed (fail-open)"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }

            scheduler.record_batch_backoff(any_spawn_success, any_concurrency_failure);

            self.process_pending_secret_requests(&mut denied_secrets)
                .await;

            let snapshot = crate::metrics::TaskGraphSnapshot::from(scheduler.graph());
            self.update_metrics(|m| {
                m.orchestration_graph = Some(snapshot);
            });

            tokio::select! {
                biased;
                () = cancel_token.cancelled() => {
                    let cancel_actions = scheduler.cancel_all();
                    if let Some(s) = self.cancel_agents_from_actions(cancel_actions) {
                        break 'tick s;
                    }
                    break 'tick zeph_orchestration::GraphStatus::Canceled;
                }
                () = scheduler.wait_event() => {}
                result = async {
                    if stdin_closed {
                        std::future::pending::<Result<Option<crate::channel::ChannelMessage>, crate::channel::ChannelError>>().await
                    } else {
                        self.channel.recv().await
                    }
                } => {
                    if let Ok(Some(msg)) = result {
                        if msg.text.trim().eq_ignore_ascii_case("/plan cancel") {
                            let _ = self.channel.send_status("Canceling plan...").await;
                            let cancel_actions = scheduler.cancel_all();
                            if let Some(s) = self.cancel_agents_from_actions(cancel_actions) {
                                break 'tick s;
                            }
                            break 'tick zeph_orchestration::GraphStatus::Canceled;
                        }
                        self.enqueue_or_merge(msg.text, vec![], msg.attachments);
                    } else {
                        let drain_actions = scheduler.tick();
                        let natural_done = self.cancel_agents_from_actions(drain_actions);

                        if let Some(status) = natural_done {
                            break 'tick status;
                        }

                        if scheduler.has_running_tasks() {
                            // Channel closed (piped stdin EOF) but sub-agents are still
                            // running. Park the recv arm and let wait_event() drive the
                            // loop until they finish naturally.
                            stdin_closed = true;
                            continue;
                        }

                        let cancel_actions = scheduler.cancel_all();
                        let n = cancel_actions
                            .iter()
                            .filter(|a| matches!(a, SchedulerAction::Cancel { .. }))
                            .count();
                        let shutdown_status = if self.channel.supports_exit() {
                            zeph_orchestration::GraphStatus::Canceled
                        } else {
                            zeph_orchestration::GraphStatus::Failed
                        };
                        tracing::warn!(
                            sub_agents = n,
                            supports_exit = self.channel.supports_exit(),
                            status = ?shutdown_status,
                            "scheduler channel closed, canceling running sub-agents"
                        );
                        self.cancel_agents_from_actions(cancel_actions);
                        break 'tick shutdown_status;
                    }
                }
                () = shutdown_signal(&mut self.lifecycle.shutdown) => {
                    let cancel_actions = scheduler.cancel_all();
                    let n = cancel_actions
                        .iter()
                        .filter(|a| matches!(a, SchedulerAction::Cancel { .. }))
                        .count();
                    tracing::warn!(sub_agents = n, "shutdown signal received, canceling running sub-agents");
                    if let Some(s) = self.cancel_agents_from_actions(cancel_actions) {
                        break 'tick s;
                    }
                    break 'tick zeph_orchestration::GraphStatus::Canceled;
                }
            }
        };

        self.process_pending_secret_requests(&mut std::collections::HashSet::new())
            .await;

        Ok(final_status)
    }

    /// Run a tool-aware LLM loop for an inline scheduled task.
    ///
    /// Unlike [`process_response_native_tools`], this is intentionally stripped of all
    /// interactive-session machinery (channel sends, doom-loop detection, summarization,
    /// learning engine, sanitizer, metrics). Inline tasks are short-lived orchestration
    /// sub-tasks that run synchronously inside the scheduler tick loop.
    pub(super) async fn run_inline_tool_loop(
        &mut self,
        prompt: &str,
        max_iterations: usize,
    ) -> Result<String, zeph_llm::LlmError> {
        use zeph_llm::provider::{ChatResponse, Message, MessagePart, Role, ToolDefinition};
        use zeph_tools::executor::ToolCall;

        let tool_defs: Vec<ToolDefinition> = self
            .tool_executor
            .tool_definitions_erased()
            .iter()
            .map(tool_execution::tool_def_to_definition)
            .collect();

        tracing::debug!(
            prompt_len = prompt.len(),
            max_iterations,
            tool_count = tool_defs.len(),
            "inline tool loop: starting"
        );

        let mut messages: Vec<Message> = vec![Message::from_legacy(Role::User, prompt)];
        let mut last_text = String::new();

        for iteration in 0..max_iterations {
            let response = self.provider.chat_with_tools(&messages, &tool_defs).await?;

            match response {
                ChatResponse::Text(text) => {
                    tracing::debug!(iteration, "inline tool loop: text response, returning");
                    return Ok(text);
                }
                ChatResponse::ToolUse {
                    text, tool_calls, ..
                } => {
                    tracing::debug!(
                        iteration,
                        tools = ?tool_calls.iter().map(|tc| &tc.name).collect::<Vec<_>>(),
                        "inline tool loop: tool use"
                    );

                    if let Some(ref t) = text {
                        last_text.clone_from(t);
                    }

                    let mut parts: Vec<MessagePart> = Vec::new();
                    if let Some(ref t) = text
                        && !t.is_empty()
                    {
                        parts.push(MessagePart::Text { text: t.clone() });
                    }
                    for tc in &tool_calls {
                        parts.push(MessagePart::ToolUse {
                            id: tc.id.clone(),
                            name: tc.name.to_string(),
                            input: tc.input.clone(),
                        });
                    }
                    messages.push(Message::from_parts(Role::Assistant, parts));

                    let mut result_parts: Vec<MessagePart> = Vec::new();
                    for tc in &tool_calls {
                        let call = ToolCall {
                            tool_id: tc.name.clone(),
                            params: match &tc.input {
                                serde_json::Value::Object(map) => map.clone(),
                                _ => serde_json::Map::new(),
                            },
                            caller_id: None,
                        };
                        let output = loop {
                            tokio::select! {
                                result = self.tool_executor.execute_tool_call_erased(&call) => {
                                    break match result {
                                        Ok(Some(out)) => out.summary,
                                        Ok(None) => "(no output)".to_owned(),
                                        Err(e) => format!("[error] {e}"),
                                    };
                                }
                                Some(event) = async {
                                    match self.mcp.elicitation_rx.as_mut() {
                                        Some(rx) => rx.recv().await,
                                        None => std::future::pending().await,
                                    }
                                } => {
                                    self.handle_elicitation_event(event).await;
                                }
                            }
                        };
                        let is_error = output.starts_with("[error]");
                        result_parts.push(MessagePart::ToolResult {
                            tool_use_id: tc.id.clone(),
                            content: output,
                            is_error,
                        });
                    }
                    messages.push(Message::from_parts(Role::User, result_parts));
                }
            }
        }

        tracing::debug!(
            max_iterations,
            last_text_empty = last_text.is_empty(),
            "inline tool loop: iteration limit reached"
        );
        Ok(last_text)
    }

    /// Bridge pending secret requests from sub-agents to the user (non-blocking, time-bounded).
    ///
    /// SEC-P1-02: explicit user confirmation is required before granting any secret to a
    /// sub-agent. Denial is the default on timeout or channel error.
    ///
    /// `denied` tracks `(handle_id, secret_key)` pairs already denied this plan execution.
    /// Re-requests for a denied pair are auto-denied without prompting the user.
    pub(super) async fn process_pending_secret_requests(
        &mut self,
        denied: &mut std::collections::HashSet<(String, String)>,
    ) {
        loop {
            let pending = self
                .orchestration
                .subagent_manager
                .as_mut()
                .and_then(zeph_subagent::SubAgentManager::try_recv_secret_request);
            let Some((req_handle_id, req)) = pending else {
                break;
            };
            let deny_key = (req_handle_id.clone(), req.secret_key.clone());
            if denied.contains(&deny_key) {
                tracing::debug!(
                    handle_id = %req_handle_id,
                    secret_key = %req.secret_key,
                    "skipping duplicate secret prompt for already-denied key"
                );
                if let Some(mgr) = self.orchestration.subagent_manager.as_mut() {
                    let _ = mgr.deny_secret(&req_handle_id);
                }
                continue;
            }
            let prompt = format!(
                "Sub-agent requests secret '{}'. Allow?{}",
                crate::text::truncate_to_chars(&req.secret_key, 100),
                req.reason
                    .as_deref()
                    .map(|r| format!(" Reason: {}", crate::text::truncate_to_chars(r, 200)))
                    .unwrap_or_default()
            );
            let approved = tokio::select! {
                result = self.channel.confirm(&prompt) => result.unwrap_or(false),
                () = tokio::time::sleep(std::time::Duration::from_mins(2)) => {
                    let _ = self.channel.send("Secret request timed out.").await;
                    false
                }
            };
            if let Some(mgr) = self.orchestration.subagent_manager.as_mut() {
                if approved {
                    let ttl = std::time::Duration::from_mins(5);
                    let key = req.secret_key.clone();
                    if mgr.approve_secret(&req_handle_id, &key, ttl).is_ok() {
                        let _ = mgr.deliver_secret(&req_handle_id, key);
                    }
                } else {
                    denied.insert(deny_key);
                    let _ = mgr.deny_secret(&req_handle_id);
                }
            }
        }
    }
}
