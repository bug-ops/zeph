// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use tokio_util::sync::CancellationToken;
use zeph_llm::provider::LlmProvider;

use super::Agent;
use super::error;
use super::shutdown_signal;
use super::tool_execution;

/// Outcome of [`Agent::run_inline_tool_loop`]: the final narrated text plus the real tool-call
/// trace observed in-loop, for verifier grounding (spec 009 § Verifier Tool-Call Grounding).
#[derive(Debug)]
pub(super) struct InlineLoopOutcome {
    /// Final narrated text (either a `ChatResponse::Text`, or the last narrated text seen
    /// before the iteration limit was reached).
    pub(super) text: String,
    /// Real tool invocations observed during the loop, in call order. Always present (never
    /// `None` at the `TaskOutcome::Completed` call site) — this path has no I/O failure mode,
    /// unlike the spawn path's transcript read.
    pub(super) tool_trace: Vec<zeph_orchestration::ToolCallSummary>,
}

/// Returns the BFS depth to pass to `lookahead_tools` for a given fidelity configuration.
///
/// When fidelity is disabled (`None` or `enabled = false`) returns `0` so the BFS
/// is skipped entirely — the resulting hints are never consumed in that state.
fn lookahead_effective_depth(fidelity: Option<&zeph_config::FidelityConfig>) -> u8 {
    fidelity.map_or(0, |c| if c.enabled { c.lookahead_depth } else { 0 })
}

/// Returns `true` if `task` carries `NetworkScope::Deny`, in which case the spawned
/// sub-agent's tool executor must be wrapped with `NetworkDenyToolExecutor` (spec
/// `069-threat-model` OQ-1). `Inherit`, `Allow`, and `None` all return `false` — only an
/// explicit `Deny` restricts network egress; the default/non-Deny path is unaffected.
///
/// Fails open (`false`) when `task` is `None` — a graph-desync task-lookup miss cannot be
/// distinguished from a genuinely scope-less task here, so this logs at `debug` for
/// observability rather than assuming `Deny` (consistent with the product's
/// network-allow-by-default model).
fn network_denied_for_task(task: Option<&zeph_orchestration::TaskNode>) -> bool {
    if task.is_none() {
        tracing::debug!("network_denied_for_task: task lookup missed, defaulting to not-denied");
    }
    matches!(
        task.and_then(|t| t.network_scope),
        Some(zeph_orchestration::NetworkScope::Deny)
    )
}

/// Reconstruct a [`zeph_orchestration::ToolCallSummary`] trace from a loaded transcript's
/// messages, pairing each `MessagePart::ToolUse` with its later `MessagePart::ToolResult` (by
/// `tool_use_id`) for the `ok` field. A `ToolUse` with no matching `ToolResult` (e.g. the
/// sub-agent was canceled mid-call) is still included, defaulting `ok` to `true` — grounding's
/// matching rule does not consult `ok` (existence, not outcome, is in scope), so this default
/// cannot cause a false grounding match/mismatch.
pub(super) fn tool_trace_from_messages(
    messages: &[zeph_llm::provider::Message],
) -> Vec<zeph_orchestration::ToolCallSummary> {
    use std::collections::HashMap;
    use zeph_llm::provider::MessagePart;

    let mut result_ok: HashMap<&str, bool> = HashMap::new();
    for msg in messages {
        for part in &msg.parts {
            if let MessagePart::ToolResult {
                tool_use_id,
                is_error,
                ..
            } = part
            {
                result_ok.insert(tool_use_id.as_str(), !is_error);
            }
        }
    }

    let mut trace = Vec::new();
    for msg in messages {
        for part in &msg.parts {
            if let MessagePart::ToolUse { id, name, input } = part {
                trace.push(zeph_orchestration::ToolCallSummary {
                    tool: name.clone(),
                    args_summary: tool_execution::summarize_tool_input(input),
                    ok: result_ok.get(id.as_str()).copied().unwrap_or(true),
                });
            }
        }
    }
    trace
}

/// Save a graph snapshot to persistent storage with a 5-second timeout.
///
/// Fail-open: errors and timeouts are logged at `warn!` level and do not abort
/// the scheduler tick. Callers that need `error!` level (authoritative terminal
/// saves) should inline their own match block.
///
/// # Note on timeout testing
///
/// This 5-second `SQLite` timeout is not exercised in unit tests because
/// `:memory:` stores do not exhibit blocking behaviour. Timeout coverage
/// requires an integration test with an artificially stalled pool.
pub(super) async fn save_graph_snapshot(
    persistence: &zeph_orchestration::GraphPersistence<
        zeph_memory::store::graph_store::TaskGraphStore,
    >,
    graph: zeph_orchestration::TaskGraph,
) {
    tracing::debug!(graph_id = %graph.id, status = %graph.status, "save_graph_snapshot: start");
    match tokio::time::timeout(std::time::Duration::from_secs(5), persistence.save(&graph)).await {
        Ok(Ok(())) => tracing::debug!(graph_id = %graph.id, "save_graph_snapshot: done"),
        Ok(Err(e)) => tracing::warn!(
            error = %e, graph_id = %graph.id,
            "graph persistence save failed (fail-open)"
        ),
        Err(_) => tracing::warn!(
            graph_id = %graph.id,
            "graph persistence save timed out after 5s (fail-open)"
        ),
    }
}

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
                    if let Some(mgr) = self.services.orchestration.subagent_manager.as_mut() {
                        let _ = mgr.cancel(&agent_handle_id).inspect_err(|e| {
                            tracing::trace!(error = %e, "cancel: agent already gone");
                        });
                    }
                }
                SchedulerAction::Done { status } => return Some(status),
                _ => {} // non_exhaustive: unrecognised variants are no-ops
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
        let task = scheduler.graph().tasks.get(task_id.index());
        let task_title = task.map_or("unknown", |t| t.title.as_str());
        let network_denied = network_denied_for_task(task);

        let provider = self.provider.clone();
        let tool_executor = Arc::clone(&self.tool_executor);
        let skills = self.filtered_skills_for(&agent_def_name);
        let cfg = self.services.orchestration.subagent_config.clone();
        let event_tx = scheduler.event_sender();
        let task_supervisor = Arc::clone(&self.runtime.lifecycle.task_supervisor);

        let mut spawn_ctx = self.build_spawn_context(&cfg);
        spawn_ctx.network_denied = network_denied;

        // Idle-timeout progress heartbeat (issue #6245, Alt-A): the driver owns creation of
        // the Arc. One clone flows into the sub-agent loop via `spawn_ctx.progress_at`
        // (`run_agent_loop` writes `monotonic_millis()` to it once per turn boundary); the
        // original is handed to `record_spawn` below on successful spawn so the scheduler's
        // `check_timeouts()` reads the same counter.
        let progress_at = Arc::new(AtomicU64::new(zeph_common::monotonic_millis()));
        spawn_ctx.progress_at = Some(Arc::clone(&progress_at));

        let mgr = self
            .services
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
                        // Spawn path: no in-loop trace available here. The transcript-derived
                        // trace is fetched later, at the SchedulerAction::Verify handler.
                        tool_trace: None,
                    },
                    Err(e) => TaskOutcome::Failed {
                        error: e.to_string(),
                    },
                };
                let tx = event_tx;
                let sup = task_supervisor.clone();
                let send_event = async move {
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
                };
                drop(sup.spawn_oneshot(
                    std::sync::Arc::from("agent.scheduler.task_event_send"),
                    move || send_event,
                ));
            }
        };

        match mgr
            .spawn_for_task(
                &agent_def_name,
                &prompt,
                provider,
                tool_executor,
                skills,
                &cfg,
                spawn_ctx,
                on_done,
            )
            .await
        {
            Ok(handle_id) => {
                *spawn_counter += 1;
                self.channel
                    .send_status_best_effort(&format!(
                        "Executing task {spawn_counter}/{task_count}: {task_title}..."
                    ))
                    .await;
                scheduler.record_spawn(task_id, handle_id, agent_def_name, Some(progress_at));
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
        let task = scheduler.graph().tasks.get(task_id.index());
        let task_title = task.map_or("unknown", |t| t.title.as_str());
        let network_denied = network_denied_for_task(task);
        self.channel
            .send_status_best_effort(&format!(
                "Executing task {spawn_counter}/{task_count} (inline): {task_title}..."
            ))
            .await;

        let handle_id = format!("__inline_{task_id}__");
        // Idle-timeout exemption (issue #6245, F2): `RunInline` tasks pass `None` for the
        // progress handle — they are never idle-tracked. Primary guard: `check_timeouts`'s
        // idle branch short-circuits on `RunningTask::last_progress_at.is_none()`, so the
        // exemption holds unconditionally. Secondary (explanatory) reason: this action runs
        // synchronously inside the current tick's action loop, so `check_timeouts` cannot
        // observe it mid-run anyway, and its completion event is sent in-band (awaited,
        // below) and drained by the next `tick()` before `check_timeouts` runs — never
        // detach that send (e.g. via `spawn_oneshot` as the spawn path's `on_done` does) or
        // a completed-but-still-`running` inline task could be spuriously idle-killed if the
        // primary guard above were ever removed.
        scheduler.record_spawn(task_id, handle_id.clone(), "__main__".to_string(), None);

        // Inject per-task execution environment so that ToolCalls built inside this
        // inline loop carry the right named env for ShellExecutor::resolve_context.
        let prev_task_env = self.services.orchestration.task_execution_env.clone();
        self.services.orchestration.task_execution_env = scheduler
            .graph()
            .tasks
            .get(task_id.index())
            .and_then(|t| t.execution_environment.clone());

        // NetworkScope::Deny (spec 069-threat-model OQ-1, #6030 S1 follow-up): unlike a
        // spawned sub-agent, a `RunInline` task executes inside this agent's own tool loop
        // and shares `self.tool_executor` directly (see `run_inline_tool_loop`'s dispatch
        // via `self.tool_executor.execute_tool_call_erased`). There is no per-spawn
        // executor to wrap, so temporarily replace `self.tool_executor` with a
        // `NetworkDenyToolExecutor` for the duration of this single inline turn, then
        // restore it unconditionally. Safe because `Agent<C>` methods take `&mut self`:
        // no concurrent task can observe or race the swap, and any sub-agent already
        // spawned holds its own `Arc` clone taken before this point, so it is unaffected.
        let prev_executor = network_denied.then(|| {
            tracing::warn!(
                %task_id,
                "RunInline task carries NetworkScope::Deny — wrapping tool_executor for this turn"
            );
            let prev = Arc::clone(&self.tool_executor);
            self.tool_executor = Arc::new(zeph_subagent::NetworkDenyToolExecutor::new(Arc::clone(
                &prev,
            )));
            prev
        });

        let event_tx = scheduler.event_sender();
        let max_iter = self.tool_orchestrator.max_iterations;
        // Per-task run_timeout override (spec-075 FR-004): `RunInline` tasks share the
        // agent's tick loop, so `check_timeouts()` cannot observe them mid-run — this
        // `select!` branch is the only enforcement point on this dispatch path. Falls
        // back to the graph-global `task_timeout_secs` default when unset, consistent
        // with `check_timeouts()`'s `effective_run_timeout` on the spawned-task path.
        let global_task_timeout_secs = self
            .services
            .orchestration
            .orchestration_config
            .task_timeout_secs;
        let effective_run_timeout_secs = scheduler
            .graph()
            .tasks
            .get(task_id.index())
            .and_then(|t| t.timeout.as_ref())
            .and_then(|t| t.run_timeout_secs)
            .unwrap_or(global_task_timeout_secs);
        let effective_run_timeout = std::time::Duration::from_secs(effective_run_timeout_secs);
        let outcome = tokio::select! {
            result = self.run_inline_tool_loop(&prompt, max_iter) => {
                match result {
                    Ok(InlineLoopOutcome { text, tool_trace }) => zeph_orchestration::TaskOutcome::Completed {
                        output: text,
                        artifacts: vec![],
                        // RunInline path: the real trace is always available (observed directly
                        // in-loop), even when empty — never None here.
                        tool_trace: Some(tool_trace),
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
            () = tokio::time::sleep(effective_run_timeout) => {
                zeph_orchestration::TaskOutcome::Failed {
                    error: format!("RunInline task exceeded run_timeout ({effective_run_timeout:?})"),
                }
            }
        };
        // Restore prior env (supports nested RunInline, though unusual in practice).
        self.services.orchestration.task_execution_env = prev_task_env;
        if let Some(prev) = prev_executor {
            self.tool_executor = prev;
        }

        let event = zeph_orchestration::TaskEvent {
            task_id,
            agent_handle_id: handle_id,
            outcome,
        };
        if let Err(e) = event_tx.send(event).await {
            tracing::warn!(%task_id, error = %e, "inline task event send failed");
        }
    }

    // SAFETY(too_many_lines): sequential scheduler event loop with 4 tokio::select! branches
    // (cancel token, scheduler tick, channel recv with /plan cancel + channel-close paths,
    // shutdown signal) — each branch requires distinct cancel/fail/ignore semantics and
    // shares the labeled `'tick` break target. Splitting branches across methods would
    // require threading `&mut DagScheduler` into futures that cross `.await` points,
    // violating Send bounds on the async trait. The per-branch dispatch helpers
    // (`handle_scheduler_spawn_action`, `handle_run_inline_action`, `cancel_agents_from_actions`)
    // already carry the extractable work; the remaining body is irreducible control flow.
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
        use zeph_orchestration::{
            EnsembleAttempt, EnsembleTracker, EnsembleVerifier, PlanVerifier, SchedulerAction,
        };

        let mut spawn_counter: usize = 0;

        let mut denied_secrets: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();

        let mut plan_verifier: Option<PlanVerifier<zeph_llm::any::AnyProvider>> = None;
        // ORCH-style deterministic verifier ensemble-merge (spec 073-orch-ensemble-merge).
        // `None` when disabled or before the first `Verify` action of the session.
        let mut ensemble_verifier: Option<EnsembleVerifier> = None;
        let mut stdin_closed = false;
        // In-flight dedupe for VerifyPredicate actions (S9): prevents double-charging
        // the LLM when tick() re-emits the same task before the previous eval completes.
        // Reset on process restart — restart-safety is provided by predicate_outcome.is_none().
        let mut in_flight_predicate_evals: std::collections::HashSet<zeph_orchestration::TaskId> =
            std::collections::HashSet::new();

        let final_status = 'tick: loop {
            let actions = scheduler.tick();

            // Update lookahead cache so prepare_context can read PAACE hints between ticks.
            // When fidelity scoring is disabled the hints are never consumed, so skip the BFS.
            let effective_depth =
                lookahead_effective_depth(self.services.memory.compaction.fidelity_config.as_ref());
            self.services.orchestration.cached_lookahead =
                zeph_orchestration::lookahead_tools(scheduler.graph(), effective_depth);

            let mut any_spawn_success = false;
            let mut any_concurrency_failure = false;
            // Set by the `Spawn` (forced-Done-on-failure) and `Done` arms below. Deferring the
            // `'tick` break until after `collect_finished_subagents()` (below the `for` loop)
            // ensures a task whose completion coincides with graph completion in the same tick —
            // the common case for the last task of a plan — still has its handle reaped instead
            // of leaking (issue #6288: an unconditional `break 'tick` here would bypass the
            // per-tick reap entirely on the terminating tick).
            let mut done_status: Option<zeph_orchestration::GraphStatus> = None;

            'actions: for action in actions {
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
                            done_status = Some(s);
                            break 'actions;
                        }
                    }
                    SchedulerAction::Cancel { agent_handle_id } => {
                        if let Some(mgr) = self.services.orchestration.subagent_manager.as_mut() {
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
                        done_status = Some(status);
                        break 'actions;
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

                        // Resolve predicate provider: predicate_provider -> orchestrator_provider
                        // -> verify_provider -> primary.
                        let predicate_provider = self
                            .services
                            .orchestration
                            .predicate_provider
                            .as_ref()
                            .or(self.services.orchestration.orchestrator_provider.as_ref())
                            .or(self.services.orchestration.verify_provider.as_ref())
                            .unwrap_or(&self.provider)
                            .clone();

                        let prior_reason = scheduler
                            .predicate_failure_reason(task_id)
                            .map(str::to_string);
                        let max_tasks =
                            self.services.orchestration.orchestration_config.max_tasks as usize;

                        let timeout_secs = self
                            .services
                            .orchestration
                            .orchestration_config
                            .predicate_timeout_secs;
                        let sanitizer: std::sync::Arc<dyn zeph_common::OutputSanitizer> =
                            std::sync::Arc::new(self.services.security.sanitizer.clone());
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
                    SchedulerAction::CheckToolOutcome {
                        task_id,
                        tool_trace,
                    } => {
                        // #6380: deterministic, always-on check (never gated on
                        // verify_completeness) — a task whose every real tool call failed
                        // (including policy_blocked denials) must not remain Completed.
                        // Same trace-resolution contract as SchedulerAction::Verify below.
                        let task = scheduler.graph().tasks.get(task_id.index()).cloned();
                        if let Some(task) = task {
                            let resolved_tool_trace: Option<
                                Vec<zeph_orchestration::ToolCallSummary>,
                            > = tool_trace.or_else(|| self.build_tool_trace_for_task(&task));
                            scheduler.correct_completed_to_failed_if_all_tool_calls_failed(
                                task_id,
                                resolved_tool_trace.as_deref(),
                            );
                        }
                    }
                    SchedulerAction::Verify {
                        task_id,
                        output,
                        tool_trace,
                    } => {
                        let verify_provider = self
                            .services
                            .orchestration
                            .verify_provider
                            .as_ref()
                            .unwrap_or(&self.provider)
                            .clone();
                        let threshold = self
                            .services
                            .orchestration
                            .orchestration_config
                            .completeness_threshold;
                        let sanitizer: std::sync::Arc<dyn zeph_common::OutputSanitizer> =
                            std::sync::Arc::new(self.services.security.sanitizer.clone());

                        let orch_config = self.services.orchestration.orchestration_config.clone();
                        let verifier = plan_verifier.get_or_insert_with(|| {
                            PlanVerifier::new(verify_provider, sanitizer, &orch_config)
                        });

                        let task = scheduler.graph().tasks.get(task_id.index()).cloned();

                        if let Some(task) = task {
                            // RunInline already carries its in-loop trace; the spawn path
                            // carries `None` here and the trace is derived from the sub-agent
                            // transcript instead (spec 009 § Verifier Tool-Call Grounding,
                            // "Implementation Surface"). Fails closed to `None` on any lookup
                            // miss — never a bogus `Some(&[])` (S3).
                            let resolved_tool_trace: Option<
                                Vec<zeph_orchestration::ToolCallSummary>,
                            > = tool_trace.or_else(|| self.build_tool_trace_for_task(&task));

                            let ensemble_cfg = &orch_config.ensemble;
                            let resolved_count = self.services.orchestration.ensemble_members.len();
                            // The odd/>=3 invariant is validated at config load for the
                            // *configured* member list (spec 073 FR-014), but bootstrap-time
                            // provider resolution can shrink the *effective* set below it
                            // (critic S1) — gate on the resolved count's shape, not merely
                            // non-empty, so a degenerate/even effective ensemble can never run.
                            let effective_ensemble_valid =
                                !resolved_count.is_multiple_of(2) && resolved_count >= 3;
                            let use_ensemble = ensemble_cfg.enabled
                                && ensemble_cfg.verify
                                && effective_ensemble_valid;

                            if ensemble_cfg.enabled
                                && ensemble_cfg.verify
                                && !effective_ensemble_valid
                            {
                                self.update_metrics(|m| {
                                    m.orchestration.ensemble_degraded_total += 1;
                                });
                                tracing::warn!(
                                    task_id = %task_id,
                                    resolved_count,
                                    configured_count = ensemble_cfg.members.len(),
                                    "ensemble effective member count is not odd/>=3 after \
                                     bootstrap resolution — falling back to single-provider \
                                     verify"
                                );
                            }

                            let result = if use_ensemble {
                                let member_timeout_secs = if ensemble_cfg.member_timeout_secs > 0 {
                                    ensemble_cfg.member_timeout_secs
                                } else {
                                    orch_config.verifier_timeout_secs
                                };
                                let ensemble_sanitizer: std::sync::Arc<
                                    dyn zeph_common::OutputSanitizer,
                                > = std::sync::Arc::new(self.services.security.sanitizer.clone());
                                let ev = ensemble_verifier.get_or_insert_with(|| {
                                    EnsembleVerifier::new(
                                        self.services.orchestration.ensemble_members.clone(),
                                        std::time::Duration::from_secs(member_timeout_secs),
                                        EnsembleTracker::new(
                                            ensemble_cfg.ema_alpha,
                                            ensemble_cfg.ema_decay,
                                            ensemble_cfg.min_observations,
                                        ),
                                    )
                                });

                                match ev
                                    .verify(
                                        &task,
                                        &output,
                                        resolved_tool_trace.as_deref(),
                                        &ensemble_sanitizer,
                                    )
                                    .await
                                {
                                    EnsembleAttempt::Merged { result, outcome } => {
                                        tracing::debug!(
                                            task_id = %task_id,
                                            complete = result.complete,
                                            confidence = result.confidence,
                                            agreement_ratio = outcome.agreement_ratio,
                                            tie_broken = outcome.tie_broken,
                                            "ensemble per-task verification result"
                                        );
                                        if let Some(ref tracker) = self.runtime.metrics.cost_tracker
                                        {
                                            for usage in ev.last_usage() {
                                                let member_provider = self
                                                    .services
                                                    .orchestration
                                                    .ensemble_members
                                                    .iter()
                                                    .find(|(name, _)| name == &usage.member);
                                                let (provider_kind, model) = member_provider
                                                    .map_or(
                                                        ("cloud", usage.member.as_str()),
                                                        |(_, p)| {
                                                            (
                                                                p.provider_kind_str(),
                                                                p.model_identifier(),
                                                            )
                                                        },
                                                    );
                                                tracker.record_usage(
                                                    &usage.member,
                                                    provider_kind,
                                                    model,
                                                    usage.input_tokens,
                                                    0,
                                                    0,
                                                    usage.output_tokens,
                                                );
                                            }
                                        }
                                        let member_stats = ev.tracker().snapshot();
                                        self.update_metrics(|m| {
                                            m.orchestration.ensemble_last_agreement_ratio =
                                                Some(outcome.agreement_ratio);
                                            m.orchestration.ensemble_member_stats = member_stats;
                                        });
                                        result
                                    }
                                    EnsembleAttempt::QuorumNotMet {
                                        responded,
                                        quorum,
                                        configured,
                                    } => {
                                        self.update_metrics(|m| {
                                            m.orchestration.ensemble_degraded_total += 1;
                                        });
                                        tracing::warn!(
                                            task_id = %task_id,
                                            responded,
                                            quorum,
                                            configured,
                                            "ensemble quorum not met — falling back to \
                                             single-provider verify"
                                        );
                                        verifier
                                            .verify(&task, &output, resolved_tool_trace.as_deref())
                                            .await
                                    }
                                }
                            } else {
                                verifier
                                    .verify(&task, &output, resolved_tool_trace.as_deref())
                                    .await
                            };

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

                            let repaired = if should_replan {
                                let max_tasks_u32 =
                                    self.services.orchestration.orchestration_config.max_tasks;
                                let max_tasks = max_tasks_u32 as usize;
                                match verifier
                                    .replan(&task, &result.gaps, scheduler.graph(), max_tasks_u32)
                                    .await
                                {
                                    Ok(new_tasks) if !new_tasks.is_empty() => {
                                        match scheduler.inject_tasks(task_id, new_tasks, max_tasks)
                                        {
                                            Ok(()) => true,
                                            Err(e) => {
                                                tracing::warn!(
                                                    error = %e,
                                                    task_id = %task_id,
                                                    "per-task replan inject_tasks failed \
                                                     (fail-open)"
                                                );
                                                false
                                            }
                                        }
                                    }
                                    Ok(_) => false,
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            task_id = %task_id,
                                            "per-task replan failed (fail-open)"
                                        );
                                        false
                                    }
                                }
                            } else {
                                false
                            };

                            // #6265: surface a visible signal when verification judged this
                            // task's output incomplete and no repair landed — worded strictly
                            // local to this task (not the whole plan), since a later
                            // whole-plan replan may still self-heal the gap (see
                            // `run_whole_plan_verify`'s own signal for the plan-level case).
                            if !result.complete && !repaired {
                                let msg = format!(
                                    "Note: task \"{}\" verification found {} unresolved gap(s) \
                                     (verification confidence {:.0}%).",
                                    task.title,
                                    result.gaps.len(),
                                    result.confidence * 100.0
                                );
                                if let Err(e) = self.channel.send(&msg).await {
                                    tracing::warn!(
                                        error = %e,
                                        task_id = %task_id,
                                        "failed to send per-task verification-incompleteness \
                                         signal"
                                    );
                                }
                            }
                        }
                    }
                    _ => {} // non_exhaustive: unrecognised variants are no-ops
                }
            }

            self.collect_finished_subagents().await;

            if let Some(status) = done_status {
                break 'tick status;
            }

            scheduler.record_batch_backoff(any_spawn_success, any_concurrency_failure);

            self.process_pending_secret_requests(&mut denied_secrets)
                .await;

            let snapshot = crate::metrics::TaskGraphSnapshot::from(scheduler.graph());
            self.update_metrics(|m| {
                m.orchestration_graph = Some(snapshot);
            });

            if scheduler.take_graph_dirty()
                && let Some(ref persistence) = self.services.orchestration.graph_persistence
            {
                let graph_clone = scheduler.graph().clone();
                save_graph_snapshot(persistence, graph_clone).await;
            }

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
                            self.channel.send_status_best_effort("Canceling plan...").await;
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
                () = shutdown_signal(&mut self.runtime.lifecycle.shutdown) => {
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

        // Clear lookahead cache so stale hints are never seen after plan completion.
        self.services.orchestration.cached_lookahead = Vec::new();

        Ok(final_status)
    }

    /// Run a tool-aware LLM loop for an inline scheduled task.
    ///
    /// Unlike [`process_response_native_tools`], this is intentionally stripped of all
    /// interactive-session machinery (channel sends, doom-loop detection, summarization,
    /// learning engine, sanitizer, metrics). Inline tasks are short-lived orchestration
    /// sub-tasks that run synchronously inside the scheduler tick loop.
    #[allow(clippy::too_many_lines)] // per-iteration secret masking (#5437) crossed the 100-line limit
    pub(super) async fn run_inline_tool_loop(
        &mut self,
        prompt: &str,
        max_iterations: usize,
    ) -> Result<InlineLoopOutcome, zeph_llm::LlmError> {
        use zeph_llm::provider::{ChatResponse, Message, MessagePart, Role, ToolDefinition};
        use zeph_orchestration::ToolCallSummary;
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
        let mut tool_trace: Vec<ToolCallSummary> = Vec::new();

        for iteration in 0..max_iterations {
            // PAAC secret masking (#5437) is structural at the provider boundary — this loop is
            // explicitly stripped of interactive-session machinery (sanitizer, PII scrub), but
            // `self.provider` still masks registered secrets transparently before dispatch.
            let response = self.provider.chat_with_tools(&messages, &tool_defs).await?;

            match response {
                ChatResponse::Text(text) => {
                    tracing::debug!(iteration, "inline tool loop: text response, returning");
                    return Ok(InlineLoopOutcome { text, tool_trace });
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
                            context: None,
                            tool_call_id: String::new(),
                            skill_name: None,
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
                                    match self.services.mcp.elicitation_rx.as_mut() {
                                        Some(rx) => rx.recv().await,
                                        None => std::future::pending().await,
                                    }
                                } => {
                                    self.handle_elicitation_event(event).await;
                                }
                            }
                        };
                        let is_error = output.starts_with("[error]");
                        tool_trace.push(ToolCallSummary {
                            tool: tc.name.to_string(),
                            args_summary: tool_execution::summarize_tool_input(&tc.input),
                            ok: !is_error,
                        });
                        result_parts.push(MessagePart::ToolResult {
                            tool_use_id: tc.id.clone(),
                            content: output,
                            is_error,
                        });
                    }
                    messages.push(Message::from_parts(Role::User, result_parts));
                }
                _ => {}
            }
        }

        tracing::debug!(
            max_iterations,
            last_text_empty = last_text.is_empty(),
            "inline tool loop: iteration limit reached"
        );
        Ok(InlineLoopOutcome {
            text: last_text,
            tool_trace,
        })
    }

    /// Build the real tool-call trace for a spawn-path task from its sub-agent transcript
    /// (spec 009 § Verifier Tool-Call Grounding, "Implementation Surface").
    ///
    /// Fails closed to `None` (never a bogus `Some(&[])`) on any lookup miss — missing
    /// `agent_id`, missing `SubAgentManager`, missing transcript directory, or a transcript
    /// read error — per the grounding trace-availability contract (S3): an unavailable trace
    /// must never masquerade as a genuinely-empty one, or an honest task hit by a transient
    /// read failure would be spuriously flagged by `PlanVerifier`'s grounding override. Uses
    /// [`TranscriptReader::load_strict`][zeph_subagent::TranscriptReader::load_strict] rather
    /// than the lenient `load` — a torn or malformed line silently dropped by the lenient
    /// reader would otherwise surface as `Some(partial)` instead of `None`, false-positiving an
    /// honest claim for the dropped tool call as a hallucination (S3 residual note).
    fn build_tool_trace_for_task(
        &self,
        task: &zeph_orchestration::TaskNode,
    ) -> Option<Vec<zeph_orchestration::ToolCallSummary>> {
        let agent_id = task.result.as_ref().and_then(|r| r.agent_id.as_deref())?;
        let mgr = self.services.orchestration.subagent_manager.as_ref()?;
        let path = mgr.transcript_path_for(&self.services.orchestration.subagent_config, agent_id);
        match zeph_subagent::TranscriptReader::load_strict(&path) {
            Ok(messages) => Some(tool_trace_from_messages(&messages)),
            Err(e) => {
                tracing::warn!(
                    task_id = %task.id,
                    agent_id = %agent_id,
                    error = %e,
                    "tool-trace transcript read failed or partial — grounding fails open for this task"
                );
                None
            }
        }
    }

    /// Reap sub-agent handles that have reached a terminal state, writing their final
    /// `TranscriptMeta` sidecar and removing them from [`zeph_subagent::SubAgentManager`].
    ///
    /// Safe to call at any point in the tick loop: [`Self::build_tool_trace_for_task`] no longer
    /// depends on handle residency, so ordering relative to `SchedulerAction::Verify` is not
    /// load-bearing here (spec 009 § Verifier Tool-Call Grounding, issue #6288). Errors are
    /// logged, not propagated — a collection failure for one task must not abort the tick loop
    /// for the rest of the plan.
    pub(super) async fn collect_finished_subagents(&mut self) {
        let Some(mgr) = &mut self.services.orchestration.subagent_manager else {
            return;
        };
        let finished: Vec<String> = mgr
            .statuses()
            .into_iter()
            .filter_map(|(id, status)| {
                matches!(
                    status.state,
                    zeph_subagent::SubAgentState::Completed
                        | zeph_subagent::SubAgentState::Failed
                        | zeph_subagent::SubAgentState::Canceled
                )
                .then_some(id)
            })
            .collect();
        for task_id in finished {
            if let Err(e) = mgr.collect(&task_id).await {
                tracing::warn!(task_id, error = %e, "failed to collect finished orchestration sub-agent");
            }
        }
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
                .services
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
                if let Some(mgr) = self.services.orchestration.subagent_manager.as_mut() {
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
            if approved {
                let ttl = std::time::Duration::from_mins(5);
                let key = req.secret_key.clone();
                let resolved = self.resolve_subagent_secret(&key);
                if let Some(mgr) = self.services.orchestration.subagent_manager.as_mut() {
                    if let Some(secret) = resolved {
                        if mgr.approve_secret(&req_handle_id, &key, ttl).is_ok()
                            && let Err(e) = mgr.deliver_secret(&req_handle_id, &key, secret)
                        {
                            tracing::warn!(error = %e, "sub-agent secret delivery failed");
                            let _ = mgr.deny_secret(&req_handle_id);
                        }
                    } else {
                        tracing::warn!(
                            "sub-agent requested secret not resolvable from vault; denying"
                        );
                        let _ = mgr.deny_secret(&req_handle_id);
                    }
                }
            } else if let Some(mgr) = self.services.orchestration.subagent_manager.as_mut() {
                denied.insert(deny_key);
                let _ = mgr.deny_secret(&req_handle_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{lookahead_effective_depth, network_denied_for_task, tool_trace_from_messages};

    #[test]
    fn fidelity_none_returns_zero() {
        assert_eq!(lookahead_effective_depth(None), 0);
    }

    #[test]
    fn fidelity_disabled_returns_zero() {
        let cfg = zeph_config::FidelityConfig {
            enabled: false,
            lookahead_depth: 3,
            ..zeph_config::FidelityConfig::default()
        };
        assert_eq!(lookahead_effective_depth(Some(&cfg)), 0);
    }

    #[test]
    fn fidelity_enabled_returns_configured_depth() {
        let cfg = zeph_config::FidelityConfig {
            enabled: true,
            lookahead_depth: 4,
            ..zeph_config::FidelityConfig::default()
        };
        assert_eq!(lookahead_effective_depth(Some(&cfg)), 4);
    }

    // ── network_denied_for_task (issue #6030) ──────────────────────────────

    fn task_with_scope(
        scope: Option<zeph_orchestration::NetworkScope>,
    ) -> zeph_orchestration::TaskNode {
        let mut node = zeph_orchestration::TaskNode::new(0, "t", "d");
        node.network_scope = scope;
        node
    }

    #[test]
    fn no_task_returns_false() {
        assert!(!network_denied_for_task(None));
    }

    #[test]
    fn missing_network_scope_returns_false() {
        let node = task_with_scope(None);
        assert!(!network_denied_for_task(Some(&node)));
    }

    #[test]
    fn inherit_scope_returns_false() {
        let node = task_with_scope(Some(zeph_orchestration::NetworkScope::Inherit));
        assert!(!network_denied_for_task(Some(&node)));
    }

    #[test]
    fn allow_scope_returns_false() {
        let node = task_with_scope(Some(zeph_orchestration::NetworkScope::Allow));
        assert!(!network_denied_for_task(Some(&node)));
    }

    #[test]
    fn deny_scope_returns_true() {
        let node = task_with_scope(Some(zeph_orchestration::NetworkScope::Deny));
        assert!(network_denied_for_task(Some(&node)));
    }

    // ── AC-8 spawn/inline trace parity + S1 fail-closed-on-partial-read regression
    //    (spec 009 § Verifier Tool-Call Grounding) ──────────────────────────────

    #[test]
    fn tool_trace_from_messages_reconstructs_tool_use_result_pairs() {
        use zeph_llm::provider::{Message, MessagePart, Role};

        let messages = vec![
            Message::from_parts(
                Role::Assistant,
                vec![MessagePart::ToolUse {
                    id: "call-1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({ "command": "cargo test" }),
                }],
            ),
            Message::from_parts(
                Role::User,
                vec![MessagePart::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: "ok".into(),
                    is_error: false,
                }],
            ),
        ];

        let trace = tool_trace_from_messages(&messages);
        assert_eq!(trace.len(), 1);
        assert_eq!(trace[0].tool, "bash");
        assert_eq!(trace[0].args_summary.as_deref(), Some("cargo test"));
        assert!(trace[0].ok);
    }

    /// Spawns a real "worker" sub-agent through [`crate::agent::Agent`]'s
    /// `AgentCommand::Background` path (the same machinery production code uses), pointed at
    /// `tmp` for transcripts, and polls until it reaches `Completed`. Returns the full agent id.
    async fn spawn_worker_and_wait_completed(
        agent: &mut crate::agent::Agent<crate::agent::agent_tests::MockChannel>,
        tmp: &std::path::Path,
    ) -> String {
        use zeph_subagent::def::{SkillFilter, SubAgentPermissions, ToolPolicy};
        use zeph_subagent::hooks::SubagentHooks;
        use zeph_subagent::{AgentCommand, SubAgentDef, SubAgentManager, SubAgentState};

        agent.services.orchestration.subagent_config.transcript_dir = Some(tmp.to_path_buf());
        agent
            .services
            .orchestration
            .subagent_config
            .transcript_enabled = true;

        let mut mgr = SubAgentManager::new(4);
        mgr.definitions_mut().push(SubAgentDef {
            name: "worker".into(),
            description: "A worker bot".into(),
            model: None,
            tools: ToolPolicy::InheritAll,
            disallowed_tools: vec![],
            permissions: SubAgentPermissions {
                max_turns: 1,
                ..SubAgentPermissions::default()
            },
            skills: SkillFilter::default(),
            system_prompt: "You are a worker.".into(),
            hooks: SubagentHooks::default(),
            memory: None,
            source: None,
            file_path: None,
        });
        agent.services.orchestration.subagent_manager = Some(mgr);

        let spawn_resp = agent
            .handle_agent_command(AgentCommand::Background {
                name: "worker".into(),
                prompt: "do a task".into(),
            })
            .await
            .expect("Background spawn must return Some");
        let short_id = spawn_resp
            .split("id: ")
            .nth(1)
            .expect("response must contain 'id: '")
            .trim_end_matches(')')
            .trim()
            .to_string();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let mgr = agent
                .services
                .orchestration
                .subagent_manager
                .as_ref()
                .unwrap();
            let statuses = mgr.statuses();
            let found = statuses.iter().find(|(id, _)| id.starts_with(&short_id));
            if let Some((id, status)) = found {
                match status.state {
                    SubAgentState::Completed => break id.clone(),
                    SubAgentState::Failed => {
                        panic!("sub-agent Failed unexpectedly: {:?}", status.last_message);
                    }
                    _ => {}
                }
            }
            assert!(
                std::time::Instant::now() <= deadline,
                "sub-agent did not complete within timeout"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Appends a real `ToolUse`("bash", `{"command": "cargo test"}`)/`ToolResult` round to the
    /// `.jsonl` transcript at `jsonl_path`, simulating a spawn-path sub-agent that actually ran
    /// a tool (the base transcript from [`spawn_worker_and_wait_completed`] has none, since
    /// `MockProvider` only emits text).
    async fn append_tool_round(jsonl_path: &std::path::Path) {
        let writer = zeph_subagent::TranscriptWriter::new(jsonl_path).unwrap();
        writer
            .append(
                1000,
                &zeph_llm::provider::Message::from_parts(
                    zeph_llm::provider::Role::Assistant,
                    vec![zeph_llm::provider::MessagePart::ToolUse {
                        id: "call-1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({ "command": "cargo test" }),
                    }],
                ),
            )
            .await
            .unwrap();
        writer
            .append(
                1001,
                &zeph_llm::provider::Message::from_parts(
                    zeph_llm::provider::Role::User,
                    vec![zeph_llm::provider::MessagePart::ToolResult {
                        tool_use_id: "call-1".into(),
                        content: "ok".into(),
                        is_error: false,
                    }],
                ),
            )
            .await
            .unwrap();
    }

    /// Drives `build_tool_trace_for_task` through both halves of its contract against a real
    /// spawned sub-agent's transcript:
    ///
    /// 1. A real transcript with a genuine `ToolUse`/`ToolResult` round resolves to
    ///    `Some(trace)` whose content matches what the inline path would have collected live
    ///    for the same tool call — this is the AC-8 spawn/inline parity gap the tester flagged
    ///    as having zero coverage.
    /// 2. Tearing that same transcript with one malformed line afterward flips the result to
    ///    `None`, not `Some(partial)` — this is the S1 regression both the tester and the critic
    ///    found independently: `TranscriptReader::load`'s lenient line-skipping previously let a
    ///    partial read masquerade as an authoritative complete trace.
    #[tokio::test]
    async fn build_tool_trace_for_task_parity_then_fails_closed_on_torn_line() {
        use crate::agent::agent_tests::*;

        let tmp = tempfile::tempdir().unwrap();
        let provider = mock_provider(vec!["task completed successfully".into()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

        let full_id = spawn_worker_and_wait_completed(&mut agent, tmp.path()).await;

        let mut task = zeph_orchestration::TaskNode::new(0, "t", "d");
        task.result = Some(zeph_orchestration::TaskResult {
            output: String::new(),
            artifacts: vec![],
            duration_ms: 0,
            agent_id: Some(full_id.clone()),
            agent_def: None,
        });

        let dir = agent
            .services
            .orchestration
            .subagent_manager
            .as_ref()
            .unwrap()
            .agent_transcript_dir(&full_id)
            .expect("transcript dir must be resolvable for a just-spawned agent")
            .to_path_buf();
        let jsonl_path = dir.join(format!("{full_id}.jsonl"));
        append_tool_round(&jsonl_path).await;

        let trace = agent
            .build_tool_trace_for_task(&task)
            .expect("intact transcript must resolve to Some(trace)");
        assert!(
            trace
                .iter()
                .any(|t| t.tool == "bash" && t.args_summary.as_deref() == Some("cargo test")),
            "spawn-path trace must reconstruct the bash/cargo-test call the inline path would \
             have collected live for the same execution: {trace:?}"
        );

        // Tear the transcript: append a raw malformed line directly (bypassing the writer's
        // serialization) to simulate a torn/partial write.
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&jsonl_path)
                .unwrap();
            writeln!(f, "not valid json").unwrap();
        }

        let trace_after_tear = agent.build_tool_trace_for_task(&task);
        assert!(
            trace_after_tear.is_none(),
            "a torn/malformed transcript line must fail closed to None, not Some(partial): \
             {trace_after_tear:?}"
        );
    }

    /// Regression test for issue #6288: `build_tool_trace_for_task` must still resolve the real
    /// trace after the sub-agent's handle has already been reaped via
    /// `SubAgentManager::collect()` — this is the exact scenario the spec's residency note used
    /// to warn against (a naive `collect()` call site degrading grounding to fail-open `None`).
    /// `transcript_path_for` (unlike `agent_transcript_dir`) is computed from `config` alone, so
    /// it must not depend on the handle still being resident in `mgr.agents`.
    #[tokio::test]
    async fn build_tool_trace_for_task_recovers_after_handle_is_collected() {
        use crate::agent::agent_tests::*;

        let tmp = tempfile::tempdir().unwrap();
        let provider = mock_provider(vec!["task completed successfully".into()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

        let full_id = spawn_worker_and_wait_completed(&mut agent, tmp.path()).await;

        let mut task = zeph_orchestration::TaskNode::new(0, "t", "d");
        task.result = Some(zeph_orchestration::TaskResult {
            output: String::new(),
            artifacts: vec![],
            duration_ms: 0,
            agent_id: Some(full_id.clone()),
            agent_def: None,
        });

        let dir = agent
            .services
            .orchestration
            .subagent_manager
            .as_ref()
            .unwrap()
            .agent_transcript_dir(&full_id)
            .expect("transcript dir must be resolvable for a just-spawned agent")
            .to_path_buf();
        let jsonl_path = dir.join(format!("{full_id}.jsonl"));
        append_tool_round(&jsonl_path).await;

        agent
            .services
            .orchestration
            .subagent_manager
            .as_mut()
            .unwrap()
            .collect(&full_id)
            .await
            .expect("collect must succeed for a completed handle");

        let trace = agent
            .build_tool_trace_for_task(&task)
            .expect("trace must still resolve to Some after the handle has been collected");
        assert!(
            trace
                .iter()
                .any(|t| t.tool == "bash" && t.args_summary.as_deref() == Some("cargo test")),
            "post-collection trace must still reconstruct the real bash/cargo-test call: {trace:?}"
        );
    }

    /// Regression test for issue #6288: every `Spawn`-dispatched task's sub-agent handle must be
    /// reaped from `SubAgentManager` once it reaches a terminal state — the orchestration
    /// dispatch path previously never called `collect()`, leaking a handle (and never writing
    /// the final `TranscriptMeta` sidecar) for every plan-executed task.
    #[tokio::test]
    async fn run_scheduler_loop_reaps_completed_spawn_dispatched_subagent() {
        use crate::agent::agent_tests::*;
        use zeph_orchestration::{DagScheduler, GraphStatus, RuleBasedRouter, TaskGraph, TaskNode};
        use zeph_subagent::{SubAgentDef, SubAgentManager};

        let mut graph = TaskGraph::new("goal");
        graph.tasks.push(TaskNode::new(0, "t", "do a task"));

        let def =
            SubAgentDef::parse("---\nname: worker\ndescription: A worker\n---\n\nDo things.\n")
                .unwrap();

        let config = zeph_config::OrchestrationConfig::default();
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(RuleBasedRouter),
            vec![def.clone()],
            None,
        )
        .unwrap();

        let provider = mock_provider(vec!["task completed successfully".into()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
        agent.services.orchestration.orchestration_config = config;

        let mut mgr = SubAgentManager::new(4);
        mgr.definitions_mut().push(def);
        agent.services.orchestration.subagent_manager = Some(mgr);

        let token = tokio_util::sync::CancellationToken::new();
        let status = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            agent.run_scheduler_loop(&mut scheduler, 1, token),
        )
        .await
        .expect("run_scheduler_loop must not hang")
        .unwrap();

        assert_eq!(status, GraphStatus::Completed);
        assert!(
            agent
                .services
                .orchestration
                .subagent_manager
                .as_ref()
                .unwrap()
                .statuses()
                .is_empty(),
            "completed spawn-dispatched sub-agent handle must be reaped, not leaked"
        );
    }

    /// Regression test for issue #6288: `collect_finished_subagents()` must be a no-op (not
    /// panic) when orchestration is not spawning any sub-agents this session, i.e.
    /// `subagent_manager` is `None`. `run_scheduler_loop` calls it unconditionally every tick
    /// regardless of whether the plan uses `Spawn` at all.
    #[tokio::test]
    async fn collect_finished_subagents_is_noop_when_subagent_manager_is_none() {
        use crate::agent::agent_tests::*;

        let provider = mock_provider(vec!["unused".into()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
        assert!(agent.services.orchestration.subagent_manager.is_none());

        agent.collect_finished_subagents().await;
    }

    /// Regression test for issue #6288: a sub-agent handle canceled mid-plan (e.g. via `/plan
    /// cancel` triggering `cancel_agents_from_actions`) must also be reaped by
    /// `collect_finished_subagents()` — the reap filter matches `Completed | Failed | Canceled`,
    /// not just `Completed`, since a Verify-arm-only or Completed-only hook would permanently
    /// leak canceled handles.
    #[tokio::test]
    async fn collect_finished_subagents_reaps_canceled_handle() {
        use crate::agent::agent_tests::*;
        use zeph_subagent::def::{SkillFilter, SubAgentPermissions, ToolPolicy};
        use zeph_subagent::hooks::SubagentHooks;
        use zeph_subagent::{SpawnContext, SubAgentDef, SubAgentManager};

        let provider = mock_provider(vec!["unused".into()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

        let def = SubAgentDef {
            name: "worker".into(),
            description: "A worker bot".into(),
            model: None,
            tools: ToolPolicy::InheritAll,
            disallowed_tools: vec![],
            permissions: SubAgentPermissions::default(),
            skills: SkillFilter::default(),
            system_prompt: "You are a worker.".into(),
            hooks: SubagentHooks::default(),
            memory: None,
            source: None,
            file_path: None,
        };

        let mut mgr = SubAgentManager::new(4);
        mgr.definitions_mut().push(def);
        let task_id = mgr
            .spawn(
                "worker",
                "do a long task",
                mock_provider(vec!["should not matter, canceled first".into()]),
                std::sync::Arc::new(MockToolExecutor::no_tools()),
                None,
                &zeph_config::SubAgentConfig::default(),
                SpawnContext::default(),
            )
            .await
            .unwrap();
        mgr.cancel(&task_id).unwrap();
        agent.services.orchestration.subagent_manager = Some(mgr);

        assert_eq!(
            agent
                .services
                .orchestration
                .subagent_manager
                .as_ref()
                .unwrap()
                .statuses()
                .iter()
                .find(|(id, _)| id == &task_id)
                .map(|(_, s)| s.state),
            Some(zeph_subagent::SubAgentState::Canceled),
            "precondition: handle must report Canceled before reaping"
        );

        agent.collect_finished_subagents().await;

        assert!(
            agent
                .services
                .orchestration
                .subagent_manager
                .as_ref()
                .unwrap()
                .statuses()
                .is_empty(),
            "canceled sub-agent handle must be reaped, not leaked"
        );
    }

    // ── #6380: spawn-path total tool-call failure must not leave a task Completed ──────

    /// Regression test for issue #6380 (the actual reported repro path): a `/plan`-orchestrated
    /// task dispatched via `Spawn` whose sub-agent's only real tool call was rejected
    /// (`is_error: true`, e.g. `policy_blocked`) must not be reported `Completed` by
    /// `run_scheduler_loop`, even with `verify_completeness` left at its default `false` — the
    /// bug this fix closes is specifically that the opt-in `Verify` action never ran for this
    /// config, so nothing ever inspected the tool outcome. This drives the real
    /// `SchedulerAction::CheckToolOutcome` handler arm end-to-end: spawn dispatch, sub-agent
    /// tool-call failure, transcript-based trace reconstruction, and the status correction.
    #[tokio::test]
    async fn run_scheduler_loop_corrects_spawn_task_to_failed_when_all_tool_calls_policy_blocked() {
        use crate::agent::agent_tests::*;
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        use zeph_llm::provider::{ChatResponse, ToolUseRequest};
        use zeph_orchestration::{
            DagScheduler, GraphStatus, RuleBasedRouter, TaskGraph, TaskNode, TaskStatus,
        };
        use zeph_subagent::{SubAgentDef, SubAgentManager};
        use zeph_tools::executor::ToolError;

        let mut graph = TaskGraph::new("goal");
        graph.tasks.push(TaskNode::new(0, "t", "do a task"));

        let def =
            SubAgentDef::parse("---\nname: worker\ndescription: A worker\n---\n\nDo things.\n")
                .unwrap();

        let config = zeph_config::OrchestrationConfig::default();
        assert!(
            !config.verify_completeness,
            "repro precondition: issue #6380 reproduces with verify_completeness at its \
             default (false) — the fix must not rely on the opt-in verify feature"
        );
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(RuleBasedRouter),
            vec![def.clone()],
            None,
        )
        .unwrap();

        // Sub-agent's LLM narrates a single tool call, then a final "done" text turn — the
        // tool call itself is rejected by the executor below, simulating a policy_blocked
        // denial (same `is_error: true` transcript shape either way; see policy_gate.rs).
        let (mock, _counter) = MockProvider::default().with_tool_use(vec![
            ChatResponse::ToolUse {
                text: None,
                tool_calls: vec![ToolUseRequest {
                    id: "call-1".into(),
                    name: "write".into(),
                    input: serde_json::json!({ "path": "out.txt" }),
                }],
                thinking_blocks: vec![],
            },
            ChatResponse::Text("done".into()),
        ]);
        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Err(ToolError::Blocked {
            command: "write".into(),
        })]);
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
        agent.services.orchestration.orchestration_config = config;

        let mut mgr = SubAgentManager::new(4);
        mgr.definitions_mut().push(def);
        agent.services.orchestration.subagent_manager = Some(mgr);

        let token = tokio_util::sync::CancellationToken::new();
        let status = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            agent.run_scheduler_loop(&mut scheduler, 1, token),
        )
        .await
        .expect("run_scheduler_loop must not hang")
        .unwrap();

        assert_eq!(
            scheduler.graph().tasks[0].status,
            TaskStatus::Failed,
            "spawn-dispatched task whose every real tool call was policy_blocked must be \
             corrected to Failed by SchedulerAction::CheckToolOutcome, not remain Completed \
             -- this is the actual issue #6380 repro path (PlanView reads per-task status)"
        );
        // Known, deliberate scope limit (critic finding S1, .local/handoff/*-critic.md):
        // the spawn-path correction is status-only and does not re-run graph completion, so
        // a single-task plan's overall GraphStatus stays Completed even though its only task
        // was just corrected to Failed. Pinning this down so a future change to propagate the
        // correction is a conscious decision, not an accidental behavior change caught here.
        assert_eq!(
            status,
            GraphStatus::Completed,
            "documents the current accepted tradeoff: post-hoc task correction does not \
             recompute graph-level status (see critic finding S1)"
        );
    }

    /// Companion no-op case for the previous test: a spawn-dispatched task whose sub-agent's
    /// tool call actually succeeded must not be touched by `CheckToolOutcome` and must remain
    /// `Completed`.
    #[tokio::test]
    async fn run_scheduler_loop_leaves_spawn_task_completed_when_tool_call_succeeds() {
        use crate::agent::agent_tests::*;
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        use zeph_llm::provider::{ChatResponse, ToolUseRequest};
        use zeph_orchestration::{
            DagScheduler, GraphStatus, RuleBasedRouter, TaskGraph, TaskNode, TaskStatus,
        };
        use zeph_subagent::{SubAgentDef, SubAgentManager};

        let mut graph = TaskGraph::new("goal");
        graph.tasks.push(TaskNode::new(0, "t", "do a task"));

        let def =
            SubAgentDef::parse("---\nname: worker\ndescription: A worker\n---\n\nDo things.\n")
                .unwrap();

        let config = zeph_config::OrchestrationConfig::default();
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(RuleBasedRouter),
            vec![def.clone()],
            None,
        )
        .unwrap();

        let (mock, _counter) = MockProvider::default().with_tool_use(vec![
            ChatResponse::ToolUse {
                text: None,
                tool_calls: vec![ToolUseRequest {
                    id: "call-1".into(),
                    name: "read".into(),
                    input: serde_json::json!({ "path": "in.txt" }),
                }],
                thinking_blocks: vec![],
            },
            ChatResponse::Text("done".into()),
        ]);
        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::with_output("read", "file contents");
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
        agent.services.orchestration.orchestration_config = config;

        let mut mgr = SubAgentManager::new(4);
        mgr.definitions_mut().push(def);
        agent.services.orchestration.subagent_manager = Some(mgr);

        let token = tokio_util::sync::CancellationToken::new();
        let status = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            agent.run_scheduler_loop(&mut scheduler, 1, token),
        )
        .await
        .expect("run_scheduler_loop must not hang")
        .unwrap();

        assert_eq!(
            scheduler.graph().tasks[0].status,
            TaskStatus::Completed,
            "a genuinely successful tool call must not be corrected away from Completed"
        );
        assert_eq!(status, GraphStatus::Completed);
    }
}
