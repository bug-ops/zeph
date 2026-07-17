// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Core tick/execution loop, event processing, and spawn record-keeping.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::{DagScheduler, RunningTask, SchedulerAction, TaskEvent, TaskOutcome};
use crate::command::TaskRef;
use crate::dag;
use crate::graph::{ExecutionMode, GraphStatus, TaskId, TaskResult, TaskStatus};
use crate::lineage::{ErrorLineage, LineageEntry, LineageKind, classify_error, now_ms};
use crate::topology::DispatchStrategy;
use zeph_subagent::SubAgentError;

/// Bundles the fields carried by `TaskOutcome::Completed`, keeping
/// `handle_completed_outcome`'s argument count under clippy's `too_many_arguments` threshold.
struct CompletedTaskData {
    output: String,
    artifacts: Vec<std::path::PathBuf>,
    tool_trace: Option<Vec<crate::verifier::ToolCallSummary>>,
}

/// `true` when a call counts toward the "did this task do work that matters" judgment used
/// by `all_tool_calls_failed` — i.e. it is *not* a call the heuristic should silently
/// ignore even if it succeeded.
///
/// This is deliberately **not** just `!is_read_only`. `zeph_common::tool_classification`'s
/// `READONLY_TOOLS` and `zeph_common::quarantine::QUARANTINE_DENIED` are not complementary:
/// `web_scrape`, `fetch`, `load_skill`, and `invoke_skill` are in *both* lists — read-only
/// for autonomy-gating purposes (#5575), yet still denied under the `quarantined` trust
/// floor. A tool call counts here if it either mutates state (`!is_read_only`) *or* is
/// itself a quarantine-denied call — a blocked `fetch`/`invoke_skill` is exactly the
/// exfiltration/side-channel class quarantine exists to stop, so a successful plain `read`
/// alongside it must not mask the denial (#6397, critic finding S2). An unclassified tool
/// (not in `READONLY_TOOLS`) already counts via `!is_read_only` — the fail-closed default is
/// unaffected by this OR.
///
/// # Known imprecision: list membership, not actual quarantine state (critic finding M1)
///
/// `zeph_common::quarantine::is_quarantine_denied` is pure list-membership, independent of
/// whether this task actually ran under the `quarantined` trust floor — `ToolCallSummary`
/// carries no such field, and no "policy-blocked vs. transient error" distinction either. So
/// a **non-quarantined** task whose only network/skill call genuinely errored (e.g. `fetch`
/// timed out) alongside a successful plain `read` is now also flagged by this heuristic, where
/// before #6397 it stayed `Completed` (a `!is_read_only`-only check would have excluded
/// `fetch`). This is a deliberate, accepted widening beyond the quarantine-only scope this fix
/// targets: the shape is narrow (requires *every* counting call to fail with only reads
/// succeeding), and such a task plausibly did fail to make progress regardless of *why* the
/// network/skill call failed. Threading real quarantine trust-floor state through
/// `ToolCallSummary` and into this heuristic would remove the imprecision but is a materially
/// larger change (new field, new production call-site plumbing from the sub-agent's trust
/// context) than this fix's scope — left as a conscious tradeoff, not silently accepted.
fn counts_toward_completion_heuristic(call: &crate::verifier::ToolCallSummary) -> bool {
    !call.is_read_only || zeph_common::quarantine::is_quarantine_denied(&call.tool)
}

/// `true` when `trace` is non-empty and the task produced zero durable work — either every
/// call in it failed (`ok == false`, including `policy_blocked` denials, issue #6380), or —
/// the mixed-trace case, issue #6397 — every call that `counts_toward_completion_heuristic`
/// flags failed while some or all of the remaining (read-only, non-quarantine-denied) calls
/// succeeded.
///
/// An empty trace is deliberately *not* treated as total failure (a task that made no tool
/// calls at all — e.g. pure reasoning/narration — is not this defect).
///
/// # Mixed-trace handling (#6397)
///
/// A trace containing at least one call that counts (see
/// `counts_toward_completion_heuristic`) is judged solely on those calls: if all of them
/// failed, the task is flagged regardless of whether any purely-read-only,
/// non-quarantine-denied call (`read`, `grep`, `list_directory`, ...) succeeded. This is the
/// common real-world shape of the underlying defect: under the `quarantined` trust floor, a
/// quarantined sub-agent that successfully reads context before every mutating or
/// quarantine-denied call is blocked previously produced an uncaught mixed trace —
/// `Completed` despite zero durable/permitted work.
///
/// A trace containing *no* counting calls (pure, non-denied reads) falls back to the
/// original "every call failed" rule, so a task that never attempted a mutating or
/// quarantine-denied call is never flagged off a single blocked read — and the pre-existing
/// full-failure case (#6380) is preserved exactly.
fn all_tool_calls_failed(trace: &[crate::verifier::ToolCallSummary]) -> bool {
    if trace.is_empty() {
        return false;
    }
    let mut counting_calls = trace
        .iter()
        .filter(|c| counts_toward_completion_heuristic(c))
        .peekable();
    if counting_calls.peek().is_some() {
        counting_calls.all(|c| !c.ok)
    } else {
        trace.iter().all(|c| !c.ok)
    }
}

impl DagScheduler {
    /// Process pending events and produce actions for the caller.
    ///
    /// Call `wait_event` after processing all actions to block until the next event.
    pub fn tick(&mut self) -> Vec<SchedulerAction> {
        if self.graph.status != GraphStatus::Running {
            return vec![SchedulerAction::Done {
                status: self.graph.status,
            }];
        }

        self.reanalyze_topology_if_dirty();

        let mut actions = self.drain_events_into_actions();

        if self.graph.status != GraphStatus::Running {
            return actions;
        }

        let timeout_actions = self.check_timeouts();
        actions.extend(timeout_actions);

        if self.graph.status != GraphStatus::Running {
            return actions;
        }

        let ready = self.ordered_ready_tasks();
        let dispatch_actions = self.dispatch_ready_tasks(ready);
        actions.extend(dispatch_actions);

        actions.extend(self.emit_pending_predicate_actions());
        actions.extend(self.check_graph_completion());

        actions
    }

    /// Drain buffered and channel events, returning all resulting actions.
    fn drain_events_into_actions(&mut self) -> Vec<SchedulerAction> {
        let mut actions = Vec::new();
        while let Some(event) = self.buffered_events.pop_front() {
            actions.extend(self.process_event(event));
        }
        while let Ok(event) = self.event_rx.try_recv() {
            actions.extend(self.process_event(event));
        }
        actions
    }

    /// Return ready task IDs ordered according to the active dispatch strategy.
    ///
    /// `CascadeAware` partitions tasks into preferred (healthy region) and deferred
    /// (cascading region). `TreeOptimized` sorts by critical-path distance descending.
    /// Sequential tasks are never reordered.
    fn ordered_ready_tasks(&mut self) -> Vec<TaskId> {
        let raw_ready = dag::ready_tasks(&self.graph);

        // CascadeAware: preferred (healthy) tasks first, deferred (cascading) tasks last.
        // Sequential tasks are exempt from reordering.
        let ready: Vec<TaskId> = if self.topology.strategy == DispatchStrategy::CascadeAware {
            if let Some(ref mut detector) = self.cascade_detector {
                let graph = &self.graph;
                let deprioritized = detector.deprioritized_tasks(graph);
                if deprioritized.is_empty() {
                    raw_ready
                } else {
                    let (preferred, deferred): (Vec<_>, Vec<_>) =
                        raw_ready.into_iter().partition(|id| {
                            let is_sequential = self.graph.tasks[id.index()].execution_mode
                                == ExecutionMode::Sequential;
                            is_sequential || !deprioritized.contains(id)
                        });
                    preferred.into_iter().chain(deferred).collect()
                }
            } else {
                raw_ready
            }
        } else {
            raw_ready
        };

        // TreeOptimized: sort by critical-path distance descending (deepest tasks first).
        if self.topology.strategy == DispatchStrategy::TreeOptimized {
            let max_depth = self.topology.depth;
            let mut sortable = ready;
            sortable.sort_by_key(|id| {
                let task_depth = self.topology.depths.get(id).copied().unwrap_or(0);
                max_depth.saturating_sub(task_depth)
            });
            sortable
        } else {
            ready
        }
    }

    /// Dispatch ready tasks up to the available concurrency slots.
    ///
    /// Concurrency is pre-enforced here (topology-aware cap) and also enforced by
    /// `SubAgentManager::spawn()` returning `ConcurrencyLimit` when slots are exhausted.
    /// Non-transient spawn failures are handled by `record_spawn_failure()`; optimistic
    /// Running marks are reverted to Ready for `ConcurrencyLimit` errors.
    fn dispatch_ready_tasks(&mut self, ready: Vec<TaskId>) -> Vec<SchedulerAction> {
        self.advance_level_barrier_if_needed();

        let mut actions = Vec::new();
        let mut slots = self.max_parallel.saturating_sub(self.running.len());

        let mut sequential_spawned_this_tick = false;
        let has_running_sequential = self
            .running
            .keys()
            .any(|tid| self.graph.tasks[tid.index()].execution_mode == ExecutionMode::Sequential);

        for task_id in ready {
            if slots == 0 {
                break;
            }

            let task = &self.graph.tasks[task_id.index()];

            // LevelBarrier: only dispatch tasks at the current level. Exception (D4,
            // spec-075 FR-D-01): a just-activated Mode-2 fallback (`Ready` with
            // `routed_from.is_some()`, set only by `dag::try_reroute`) bypasses the
            // level gate. It must dispatch immediately regardless of `current_level` —
            // waiting for the barrier to reach its depth-0 level again could stall
            // indefinitely on unrelated deeper levels. This is safe because `validate`
            // forces every route_to target to have an empty `depends_on`: it has no
            // prerequisites, so dispatching it out-of-level can never run ahead of
            // anything it depends on.
            let is_activated_fallback = task.routed_from.is_some();
            if self.topology.strategy == DispatchStrategy::LevelBarrier && !is_activated_fallback {
                let task_depth = self
                    .topology
                    .depths
                    .get(&task_id)
                    .copied()
                    .unwrap_or(usize::MAX);
                if task_depth != self.current_level {
                    continue;
                }
            }

            // Sequential tasks: only one may run at a time within the scheduler.
            // Independent sequential tasks in separate DAG branches are still
            // serialized here (they share exclusive-resource intent by annotation).
            if task.execution_mode == ExecutionMode::Sequential {
                if sequential_spawned_this_tick || has_running_sequential {
                    continue;
                }
                sequential_spawned_this_tick = true;
            }

            let Some(agent_def_name) = self.router.route(task, &self.available_agents) else {
                tracing::debug!(
                    task_id = %task_id,
                    title = %task.title,
                    "no agent available, routing task to main agent inline"
                );
                let prompt = self.build_task_prompt(task);
                self.graph.tasks[task_id.index()].status = TaskStatus::Running;
                actions.push(SchedulerAction::RunInline { task_id, prompt });
                slots -= 1;
                continue;
            };

            // Admission control: check per-provider concurrency limit before dispatching.
            // Resolve the provider name from agent_provider_map (keyed by agent def name).
            // agent_hint and agent_def_name are sub-agent names, not provider names — using
            // them directly as gate keys would silently bypass admission (C3 fix).
            if let Some(ref gate) = self.admission_gate {
                let provider_key: Option<&str> = self
                    .agent_provider_map
                    .get(&agent_def_name)
                    .map(String::as_str);
                if let Some(key) = provider_key.filter(|k| gate.has_gate(k)) {
                    if let Some(permit) = gate.try_acquire(key) {
                        self.pending_permits.insert(task_id, permit);
                    } else {
                        tracing::debug!(
                            task_id = %task_id,
                            provider = %key,
                            agent = %agent_def_name,
                            "admission gate saturated, deferring task to next tick"
                        );
                        self.consecutive_spawn_failures =
                            self.consecutive_spawn_failures.saturating_add(1);
                        continue;
                    }
                }
                // No provider mapping → agent inherits parent provider → ungated.
            }

            let prompt = self.build_task_prompt(task);

            // Mark task as Running optimistically (before record_spawn is called).
            self.graph.tasks[task_id.index()].status = TaskStatus::Running;

            actions.push(SchedulerAction::Spawn {
                task_id,
                agent_def_name,
                prompt,
            });
            slots -= 1;
        }

        actions
    }

    /// Emit `VerifyPredicate` actions for completed tasks whose predicate is unresolved.
    ///
    /// Idempotent — re-emitted every tick until `record_predicate_outcome()` populates
    /// `predicate_outcome`. The caller deduplicates in-flight evaluations (per-process
    /// `HashSet` in `scheduler_loop.rs`). S9 invariant: observation must not be gated on
    /// the replan budget — `max_replans=0` still emits `Verify`.
    fn emit_pending_predicate_actions(&self) -> Vec<SchedulerAction> {
        if !self.verify_predicate_enabled {
            return Vec::new();
        }
        self.graph
            .tasks
            .iter()
            .filter_map(|task| {
                if task.status == TaskStatus::Completed
                    && let (Some(predicate), None) =
                        (&task.verify_predicate, &task.predicate_outcome)
                {
                    let output = task
                        .result
                        .as_ref()
                        .map_or_else(String::new, |r| r.output.clone());
                    Some(SchedulerAction::VerifyPredicate {
                        task_id: task.id,
                        predicate: predicate.clone(),
                        output,
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    /// Wait for the next event from a running sub-agent.
    ///
    /// Buffers the received event for processing in the next [`DagScheduler::tick`] call.
    /// Returns immediately — sleeping for the current deferral backoff — when no tasks
    /// are running. Uses a deadline derived from the nearest task timeout so that
    /// periodic timeout checking occurs even when no events arrive.
    #[tracing::instrument(name = "orchestration.scheduler.wait_event", skip(self), fields(running = self.running.len()))]
    pub async fn wait_event(&mut self) {
        if self.running.is_empty() {
            tokio::time::sleep(self.current_deferral_backoff()).await;
            return;
        }

        // Find the nearest timeout deadline among running tasks, using each task's
        // per-task effective run-timeout override when set.
        let nearest_timeout = self
            .running
            .iter()
            .map(|(id, r)| {
                self.effective_run_timeout(*id)
                    .checked_sub(r.started_at.elapsed())
                    .unwrap_or(Duration::ZERO)
            })
            .min()
            .unwrap_or(Duration::from_secs(1));

        // Clamp to at least 100 ms to avoid busy-looping.
        let wait_duration = nearest_timeout.max(Duration::from_millis(100));

        tokio::select! {
            Some(event) = self.event_rx.recv() => {
                // SEC-ORCH-02: guard against unbounded buffer growth. Use total task
                // count rather than max_parallel so that parallel bursts exceeding
                // max_parallel do not cause premature event drops.
                if self.buffered_events.len() >= self.graph.tasks.len() * 2 {
                    // PERF-SCHED-02: log at error level — a dropped completion event
                    // leaves a task stuck in Running until its timeout fires.
                    if let Some(dropped) = self.buffered_events.pop_front() {
                        tracing::error!(
                            task_id = %dropped.task_id,
                            buffer_len = self.buffered_events.len(),
                            "event buffer saturated; completion event dropped — task may \
                             remain Running until timeout"
                        );
                    }
                }
                self.buffered_events.push_back(event);
            }
            () = tokio::time::sleep(wait_duration) => {}
        }
    }

    /// Record that a spawn action was successfully executed.
    ///
    /// Called by the caller after successfully spawning via `SubAgentManager`.
    ///
    /// Resets `consecutive_spawn_failures` to 0 as a "spawn succeeded = scheduler healthy"
    /// signal. This is intentionally separate from the batch-level backoff in
    /// [`DagScheduler::record_batch_backoff`]: `record_spawn` provides an immediate reset on the first
    /// success within a batch, while [`DagScheduler::record_batch_backoff`] governs the tick-granular
    /// failure counter used for exponential wait backoff.
    ///
    /// `last_progress_at` is the progress-heartbeat handle (issue #6245) for idle-timeout
    /// detection: `Some(handle)` for normally-dispatched tasks (the caller must pass the same
    /// `Arc` whose clone was threaded into the sub-agent loop via
    /// `SpawnContext::progress_at`), or `None` for tasks that are never idle-tracked (the
    /// `RunInline` dispatch path — see its call site for why `None` is safe there).
    pub fn record_spawn(
        &mut self,
        task_id: TaskId,
        agent_handle_id: String,
        agent_def_name: String,
        last_progress_at: Option<Arc<AtomicU64>>,
    ) {
        self.consecutive_spawn_failures = 0;
        self.graph.tasks[task_id.index()].assigned_agent = Some(agent_handle_id.clone());
        let admission_permit = self.pending_permits.remove(&task_id);
        self.running.insert(
            task_id,
            RunningTask {
                agent_handle_id,
                agent_def_name,
                started_at: std::time::Instant::now(),
                admission_permit,
                last_progress_at,
            },
        );
    }

    /// Handle a failed spawn attempt.
    ///
    /// If the error is a transient concurrency-limit rejection, reverts the task from
    /// Running back to `Ready` so the next [`DagScheduler::tick`] can retry the spawn when a slot opens.
    /// Otherwise, marks the task as `Failed` and propagates failure.
    /// Returns any cancel actions needed.
    ///
    /// # Errors (via returned actions)
    ///
    /// Propagates failure per the task's effective `FailureStrategy`.
    pub fn record_spawn_failure(
        &mut self,
        task_id: TaskId,
        error: &SubAgentError,
    ) -> Vec<SchedulerAction> {
        // Release any pending admission permit regardless of error type (C2 fix).
        // The permit was inserted by dispatch_ready_tasks before the spawn attempt.
        // On failure it must be dropped here to free the provider slot.
        self.pending_permits.remove(&task_id);

        // Transient condition: the SubAgentManager rejected the spawn because all
        // concurrency slots are occupied. Revert to Ready so the next tick retries.
        // consecutive_spawn_failures is updated batch-wide by record_batch_backoff().
        if let SubAgentError::ConcurrencyLimit { active, max } = error {
            tracing::warn!(
                task_id = %task_id,
                active,
                max,
                next_backoff_ms = self.current_deferral_backoff().as_millis(),
                "concurrency limit reached, deferring task to next tick"
            );
            self.graph.tasks[task_id.index()].status = TaskStatus::Ready;
            return Vec::new();
        }

        // SEC-ORCH-04: truncate error to avoid logging sensitive internal details.
        let error_excerpt: String = error.to_string().chars().take(512).collect();
        tracing::warn!(
            task_id = %task_id,
            error = %error_excerpt,
            "spawn failed, marking task failed"
        );
        self.graph_dirty = true;
        self.graph.tasks[task_id.index()].status = TaskStatus::Failed;
        // Populate `.result` with the spawn error so Mode-2 `routed_from` prompt injection
        // (router.rs's `build_task_prompt`) has real content to surface, and so
        // `finalize_plan_failed`'s error-message formatting doesn't fall back to
        // "unknown error". No agent was ever spawned, so `agent_id`/`agent_def`/`duration_ms`
        // stay at their zero values.
        self.graph.tasks[task_id.index()].result = Some(TaskResult {
            output: error_excerpt,
            artifacts: Vec::new(),
            duration_ms: 0,
            agent_id: None,
            agent_def: None,
        });
        let cancel_ids = dag::propagate_failure(&mut self.graph, task_id, &self.topology.rev_adj);
        let mut actions = Vec::new();
        for cancel_task_id in cancel_ids {
            if let Some(running) = self.running.remove(&cancel_task_id) {
                actions.push(SchedulerAction::Cancel {
                    agent_handle_id: running.agent_handle_id,
                });
            }
        }
        if self.graph.status != GraphStatus::Running {
            self.graph.finished_at = Some(crate::graph::chrono_now());
            actions.push(SchedulerAction::Done {
                status: self.graph.status,
            });
        }
        actions
    }

    /// Update the batch-level backoff counter after processing a full tick's spawn batch.
    ///
    /// With parallel dispatch a single tick may produce N Spawn actions. Individual
    /// per-spawn counter updates would miscount concurrent rejections as "consecutive"
    /// failures. This method captures the batch semantics instead:
    /// - If any spawn succeeded → reset the counter (scheduler is healthy).
    /// - Else if any spawn hit `ConcurrencyLimit` → this entire tick was a deferral tick.
    /// - If neither → no spawns were attempted; counter unchanged.
    pub fn record_batch_backoff(&mut self, any_success: bool, any_concurrency_failure: bool) {
        if any_success {
            self.consecutive_spawn_failures = 0;
        } else if any_concurrency_failure {
            self.consecutive_spawn_failures = self.consecutive_spawn_failures.saturating_add(1);
        }
    }

    /// Cancel all running tasks (for user-initiated plan cancellation).
    ///
    /// # Warning: Cooperative Cancellation
    ///
    /// Cancellation is cooperative and asynchronous. Tool operations (file writes, shell
    /// executions) in progress at the time of cancellation complete before the agent loop
    /// checks the cancellation token. Callers should inspect the task graph state and clean
    /// up partially-written artifacts manually.
    pub fn cancel_all(&mut self) -> Vec<SchedulerAction> {
        self.graph_dirty = true;
        self.graph.status = GraphStatus::Canceled;
        self.graph.finished_at = Some(crate::graph::chrono_now());

        // Drain running map first to avoid split borrow issues (M3).
        let running: Vec<(TaskId, RunningTask)> = self.running.drain().collect();
        let mut actions: Vec<SchedulerAction> = running
            .into_iter()
            .map(|(task_id, r)| {
                self.graph.tasks[task_id.index()].status = TaskStatus::Canceled;
                SchedulerAction::Cancel {
                    agent_handle_id: r.agent_handle_id,
                }
            })
            .collect();

        for task in &mut self.graph.tasks {
            if !task.status.is_terminal() {
                task.status = TaskStatus::Canceled;
            }
        }

        actions.push(SchedulerAction::Done {
            status: GraphStatus::Canceled,
        });
        actions
    }

    /// Compute the current deferral backoff with exponential growth capped at 5 seconds.
    ///
    /// Each consecutive spawn failure due to concurrency limits doubles the base backoff.
    fn current_deferral_backoff(&self) -> Duration {
        const MAX_BACKOFF: Duration = Duration::from_secs(5);
        let multiplier = 1u32
            .checked_shl(self.consecutive_spawn_failures.min(10))
            .unwrap_or(u32::MAX);
        self.deferral_backoff
            .saturating_mul(multiplier)
            .min(MAX_BACKOFF)
    }

    /// Process a single `TaskEvent` and return any cancel actions needed.
    fn process_event(&mut self, event: TaskEvent) -> Vec<SchedulerAction> {
        let TaskEvent {
            task_id,
            agent_handle_id,
            outcome,
        } = event;

        let Some((duration_ms, agent_def_name)) =
            self.consume_running_for_event(task_id, &agent_handle_id)
        else {
            return Vec::new();
        };

        match outcome {
            TaskOutcome::Completed {
                output,
                artifacts,
                tool_trace,
            } => self.handle_completed_outcome(
                task_id,
                agent_handle_id,
                agent_def_name,
                duration_ms,
                CompletedTaskData {
                    output,
                    artifacts,
                    tool_trace,
                },
            ),
            TaskOutcome::Failed { error } => self.handle_failed_outcome(task_id, &error),
            TaskOutcome::Handoff { output, goto } => self.handle_handoff_outcome(
                task_id,
                agent_handle_id,
                agent_def_name,
                duration_ms,
                output,
                &goto,
            ),
        }
    }

    /// Validate and remove a task from the running map; return duration and agent def name.
    ///
    /// Returns `None` and logs a warning when the event is stale (wrong handle) or the
    /// task is not in the running map at all. The C1 fix: duration is computed before
    /// the running entry is removed.
    fn consume_running_for_event(
        &mut self,
        task_id: TaskId,
        agent_handle_id: &str,
    ) -> Option<(u64, Option<String>)> {
        match self.running.get(&task_id) {
            Some(running) if running.agent_handle_id != agent_handle_id => {
                tracing::warn!(
                    task_id = %task_id,
                    expected = %running.agent_handle_id,
                    got = %agent_handle_id,
                    "discarding stale event from previous agent incarnation"
                );
                return None;
            }
            None => {
                tracing::debug!(
                    task_id = %task_id,
                    agent_handle_id = %agent_handle_id,
                    "ignoring event for task not in running map"
                );
                return None;
            }
            Some(_) => {}
        }

        let duration_ms = self.running.get(&task_id).map_or(0, |r| {
            u64::try_from(r.started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
        });
        let agent_def_name = self.running.get(&task_id).map(|r| r.agent_def_name.clone());

        self.running.remove(&task_id);

        Some((duration_ms, agent_def_name))
    }

    /// Apply the Completed outcome branch: update graph, unblock downstream tasks, emit actions.
    fn handle_completed_outcome(
        &mut self,
        task_id: TaskId,
        agent_handle_id: String,
        agent_def_name: Option<String>,
        duration_ms: u64,
        completed: CompletedTaskData,
    ) -> Vec<SchedulerAction> {
        let CompletedTaskData {
            output,
            artifacts,
            tool_trace,
        } = completed;

        // #6380/#6397: when the real tool-call trace is already known synchronously (RunInline
        // dispatch path — always `Some`, per `TaskOutcome::Completed`'s doc comment) and
        // `all_tool_calls_failed` flags it, route through the existing failure machinery
        // instead of marking the task Completed. This branches out before any
        // Completed-side-effect runs (cascade record_outcome, downstream unblocking), so
        // there is no double-count risk here — unlike the spawn dispatch path, whose
        // tool_trace is always `None` at this call site and is corrected post-hoc via the
        // `CheckToolOutcome` action emitted below instead (see
        // `DagScheduler::correct_completed_to_failed_if_all_tool_calls_failed` and
        // `DagScheduler::propagate_corrected_task_failure`, #6396, for why that two-step
        // correction is safe without double-recording this task's own cascade outcome).
        if let Some(trace) = tool_trace.as_ref()
            && all_tool_calls_failed(trace)
        {
            let error = format!(
                "all {} tool call(s) in this task failed or were policy-blocked; \
                 narration: {output}",
                trace.len()
            );
            return self.handle_failed_outcome(task_id, &error);
        }

        self.graph_dirty = true;
        self.graph.tasks[task_id.index()].status = TaskStatus::Completed;
        self.graph.tasks[task_id.index()].result = Some(TaskResult {
            output: output.clone(),
            artifacts,
            duration_ms,
            agent_id: Some(agent_handle_id),
            agent_def: agent_def_name,
        });

        // MVP budget check: warn-only. Hard enforcement requires per-task CostTracker
        // scoping (future work). We use duration_ms as a rough cost signal.
        let effective_budget = self.graph.tasks[task_id.index()]
            .token_budget_cents
            .unwrap_or(self.default_task_budget_cents);
        if effective_budget > 0.0 {
            // 1 cent per second is a conservative placeholder until real cost attribution lands.
            #[allow(clippy::cast_precision_loss)]
            let estimated_cents = duration_ms as f64 / 1_000.0;
            if estimated_cents > effective_budget {
                tracing::warn!(
                    task_id = %task_id,
                    estimated_cents,
                    budget_cents = effective_budget,
                    "task exceeded token budget (warn-only; hard enforcement is future work)"
                );
            }
        }

        self.lineage_chains.remove(&task_id);

        if let Some(ref mut detector) = self.cascade_detector {
            detector.record_outcome(task_id, true, &self.graph);
        }

        // Mark newly unblocked tasks as Ready.
        // Downstream tasks are unblocked immediately — verification does not gate dispatch.
        let newly_ready = dag::ready_tasks(&self.graph);
        for ready_id in newly_ready {
            if self.graph.tasks[ready_id.index()].status == TaskStatus::Pending {
                self.graph.tasks[ready_id.index()].status = TaskStatus::Ready;
            }
        }

        // #6380: always request a deterministic tool-outcome check — unlike Verify below,
        // this is cheap (no LLM call) and must run regardless of verify_completeness, since
        // that flag defaults to false and the defect this guards against has nothing to do
        // with completeness verification.
        let mut actions = vec![SchedulerAction::CheckToolOutcome {
            task_id,
            tool_trace: tool_trace.clone(),
        }];

        // Emit Verify action when verify_completeness is enabled.
        // The replan budget is enforced inside inject_tasks() — the observation
        // (emitting Verify) must not be gated on the mutation budget, or tasks
        // after budget exhaustion never receive verification at all.
        // max_replans=0 still emits Verify; gaps are logged only (no inject_tasks call).
        if self.verify_completeness {
            actions.push(SchedulerAction::Verify {
                task_id,
                output,
                tool_trace,
            });
        }
        actions
    }

    /// Post-hoc correction for a `Completed` task whose real tool-call trace shows zero
    /// durable/permitted work was done — every call failed (including `policy_blocked`
    /// denials, issue #6380), or every call that counts toward the heuristic (mutating, or
    /// quarantine-denied even if classified read-only — see `counts_toward_completion_heuristic`)
    /// failed while some purely-read-only, non-quarantine-denied calls succeeded (the
    /// mixed-trace case, issue #6397). No-op (returns `false`) when `tool_trace` is `None`,
    /// empty, does not meet that condition, or when
    /// the task is no longer `Completed` (already corrected, or moved on to some other
    /// status by a later transition — never clobber that).
    ///
    /// Callers that get `true` back should follow up with
    /// [`Self::propagate_corrected_task_failure`] to cancel already-unblocked dependents and
    /// recompute `GraphStatus` (issue #6396) — this method itself only flips `TaskStatus` and
    /// annotates `TaskResult::output`.
    ///
    /// # Design note: why this is not a `handle_failed_outcome` reuse
    ///
    /// This method is called from the `SchedulerAction::CheckToolOutcome` handler, which
    /// always runs *after* `handle_completed_outcome`'s full `Completed` side-effect set has
    /// already executed for this task: `cascade_detector.record_outcome` was already called
    /// with `succeeded = true`, and downstream tasks were already unblocked to `Ready` (that
    /// unblocking is intentionally not gated on any later verification — see the identical,
    /// pre-existing precedent for `SchedulerAction::Verify` at the `Completed` transition).
    /// Routing this correction through `handle_failed_outcome` instead would double-record
    /// this task in [`crate::cascade::CascadeDetector`]'s `RegionHealth` (once as success,
    /// once as failure) and could trigger a fan-out/chain cascade abort off that stale
    /// dual-counted state — skewing failure-rate metrics that `FailureStrategy` and
    /// whole-plan verify (#6379) rely on for accuracy, for a comparatively rare correction
    /// path. So this method deliberately only flips `TaskStatus` and annotates
    /// `TaskResult::output`; it does not touch `cascade_detector`, `lineage_chains`, or any
    /// retry/abort machinery. Downstream propagation is a separate, deliberate step — see
    /// [`Self::propagate_corrected_task_failure`]'s doc comment for why calling
    /// `dag::propagate_failure` there carries no double-counting risk.
    ///
    /// (The synchronously-available `RunInline` case does not go through this method at all —
    /// `handle_completed_outcome` detects total tool-call failure *before* running any
    /// `Completed` side effect and routes straight to `handle_failed_outcome`, so no
    /// double-counting question arises there.)
    pub fn correct_completed_to_failed_if_all_tool_calls_failed(
        &mut self,
        task_id: TaskId,
        tool_trace: Option<&[crate::verifier::ToolCallSummary]>,
    ) -> bool {
        let Some(trace) = tool_trace else {
            return false;
        };
        if !all_tool_calls_failed(trace) {
            return false;
        }
        let Some(task) = self.graph.tasks.get_mut(task_id.index()) else {
            return false;
        };
        if task.status != TaskStatus::Completed {
            return false;
        }

        tracing::warn!(
            task_id = %task_id,
            tool_call_count = trace.len(),
            "correcting task status Completed -> Failed: all tool calls failed or were \
             policy-blocked (#6380/#6397)"
        );
        task.status = TaskStatus::Failed;
        if let Some(result) = task.result.as_mut() {
            result.output = format!(
                "{} [corrected: all {} tool call(s) failed or were policy-blocked]",
                result.output,
                trace.len()
            );
        }
        self.graph_dirty = true;
        true
    }

    /// Apply the Handoff outcome branch (spec-080, GitHub #6363): mark the emitting node
    /// terminal `Completed`, then attempt the routed handoff via pure `dag::try_handoff`.
    ///
    /// Marking `task_id` `Completed` **before** calling `try_handoff` is the terminal-node
    /// invariant (spec-080 §6 Always / design-review F4): it is what makes the forward-only
    /// check inside `try_handoff` a sound livelock guard, since every `Command` hop then
    /// consumes exactly one not-yet-terminal node. This call site is the chosen enforcement
    /// point — see this crate's handoff notes for the alternative considered
    /// (`zeph-core` marking terminal before emitting the event) and why this site was
    /// preferred.
    ///
    /// A `try_handoff` rejection (invalid target, unsatisfied deps, exhausted budget, live
    /// `route_to` reservation) does not change `task_id`'s own outcome — it stays
    /// `Completed` with its real output preserved; only the requested extra routing never
    /// activates. No store I/O occurs here — `update` was already sanitized and persisted
    /// by the zeph-core produce-side seam before this event was sent (write-before-send,
    /// NFR-PERF-03); this function only ever sees `goto`.
    ///
    /// # Loud rejection (critic finding C1)
    ///
    /// A rejection is recorded on `task_id.handoff_rejected` (graph-visible, persisted)
    /// and logged at `error!`, not just `warn!` — the consume side must be exactly as
    /// loud about a dropped routing intent as the produce side is about a malformed
    /// Command block (FR-B-009's "never silently" posture applies on both sides of the
    /// seam). This does **not** roll back the already-persisted `update` write: by the
    /// time this function runs, that write completed and its `TaskEvent` was already
    /// sent (write-before-send is unconditional, independent of what `try_handoff` later
    /// decides). Rolling it back would require zeph-orchestration to trigger a
    /// zeph-memory store operation, which the binding layering invariant (spec-080 §6
    /// Always/Never — this crate never depends on `zeph-memory`) forbids. The dangling
    /// `orch/{graph_id}` entry is therefore an accepted, documented tradeoff: its content
    /// remains readable by any node that later reads `<shared-state>` in this graph, it
    /// is simply not the entry that was supposed to signal a completed handoff step.
    ///
    /// # Known gap: no completeness verification (M2, review finding)
    ///
    /// Unlike [`Self::handle_completed_outcome`], this method never emits
    /// `SchedulerAction::Verify` — a Command-handoff node's "I'm done" claim skips the
    /// completeness-check/replan pass an ordinary `Completed` node gets. Identified during
    /// code review as a real, non-blocking design gap (the feature is opt-in and
    /// default-off) and deliberately deferred rather than expanding this PR's scope
    /// further: fixing it needs `TaskOutcome::Handoff` to carry `tool_trace` first (it
    /// doesn't today, unlike `Completed`), plus a decision on whether `Verify`'s
    /// replan/`inject_tasks` side effects are sound for a node that already named its own
    /// next hop. Tracked as GitHub #6394.
    fn handle_handoff_outcome(
        &mut self,
        task_id: TaskId,
        agent_handle_id: String,
        agent_def_name: Option<String>,
        duration_ms: u64,
        output: String,
        goto: &TaskRef,
    ) -> Vec<SchedulerAction> {
        self.graph_dirty = true;
        self.graph.tasks[task_id.index()].status = TaskStatus::Completed;
        self.graph.tasks[task_id.index()].result = Some(TaskResult {
            output,
            artifacts: Vec::new(),
            duration_ms,
            agent_id: Some(agent_handle_id),
            agent_def: agent_def_name,
        });

        self.lineage_chains.remove(&task_id);

        if let Some(ref mut detector) = self.cascade_detector {
            detector.record_outcome(task_id, true, &self.graph);
        }

        match dag::try_handoff(&mut self.graph, task_id, goto, self.max_handoffs) {
            Ok(target) => {
                tracing::info!(
                    task_id = %task_id,
                    target = %target,
                    "orchestration.scheduler.handoff: Command handoff routed"
                );
            }
            Err(error) => {
                // Loud, not silent (critic C1): error-level log + a graph-visible,
                // persisted field. See this method's doc comment for why the
                // already-written store `update` is not rolled back.
                self.graph.tasks[task_id.index()].handoff_rejected = Some(error.to_string());
                tracing::error!(
                    task_id = %task_id,
                    %error,
                    "orchestration.scheduler.handoff: Command handoff rejected — routing \
                     intent dropped, node stays Completed"
                );
            }
        }

        // Mark newly unblocked tasks as Ready — mirrors handle_completed_outcome. Covers
        // both the activated handoff target's own dependents (if any) and any unrelated
        // dependent of task_id that does not participate in the handoff at all.
        let newly_ready = dag::ready_tasks(&self.graph);
        for ready_id in newly_ready {
            if self.graph.tasks[ready_id.index()].status == TaskStatus::Pending {
                self.graph.tasks[ready_id.index()].status = TaskStatus::Ready;
            }
        }

        Vec::new()
    }

    /// Propagate a just-applied
    /// [`Self::correct_completed_to_failed_if_all_tool_calls_failed`] correction to the rest
    /// of the graph (issue #6396): cancels dependents that were already unblocked to
    /// `Running` before the correction landed, and recomputes `GraphStatus`.
    ///
    /// Callers must invoke this only immediately after
    /// `correct_completed_to_failed_if_all_tool_calls_failed` returned `true` for the same
    /// `task_id` in the same tick — both `dag::propagate_failure` and
    /// `dag::propagate_failure_forced_terminal` no-op unless the task's status is already
    /// `Failed`.
    ///
    /// # `Retry`/`Ask` and `Abort`-with-recovery are special-cased to forced propagation
    ///
    /// `Skip` configurations always call [`dag::propagate_failure`] directly — the same
    /// function `handle_failed_outcome` uses for the `RunInline` path, giving the spawn path
    /// parity with it. `Skip`'s arm never calls `try_recover`/`try_reroute` (it goes straight
    /// to `skip_subtree`), so it never resurrects the failed task and is safe to apply here
    /// unmodified regardless of any `recovery` configuration.
    ///
    /// `Retry` and `Ask` *always* call `dag::propagate_failure_forced_terminal` instead — see
    /// critic finding S1 (2026-07-17): `propagate_failure`'s `Retry` branch resurrects the
    /// task to `Ready` for redispatch, and `Ask` pauses the whole graph, both of which assume
    /// the task never produced output. That assumption is false here: this task was already
    /// `Completed` (`cascade_detector.record_outcome(true)` already ran, dependents already
    /// unblocked) *before* the correction landed. A `Retry` redispatch would trigger a second,
    /// redundant sub-agent run whose eventual completion re-invokes `record_outcome`,
    /// double-counting `RegionHealth` — and cannot repair the plan anyway, since
    /// already-unblocked dependents are not rolled back (see "Remaining limitation" below).
    /// `Ask` would pause the entire plan post-hoc for a task whose consequences already
    /// landed.
    ///
    /// `Abort` (the default strategy) is safe to route through `propagate_failure` unmodified
    /// **only when the task has no `recovery.state_injection` configured**. `propagate_failure`'s
    /// `Abort` arm calls `try_recover` *before* terminal-failing the graph — see critic finding
    /// S1-residual (2026-07-17): when `state_injection` is set, `try_recover` flips this
    /// just-corrected task straight back to `Completed` (with injected output that no
    /// already-unblocked dependent will ever see), silently undoing the correction exactly like
    /// the `Retry`/`Ask` hazard above. So an `Abort`-configured task with `state_injection` also
    /// routes through `dag::propagate_failure_forced_terminal`, which skips `try_recover`
    /// entirely. `try_reroute` (Mode-2 `route_to`) is *not* part of this hazard — unlike
    /// `try_recover` it never touches the source task's own status, so a plain `Abort` +
    /// `route_to` (no `state_injection`; the two are mutually exclusive per `RecoveryAction`'s
    /// doc) still goes through unmodified `propagate_failure` and may activate a Mode-2
    /// fallback off this correction, which is an accepted, deliberate tradeoff ("on failure,
    /// run the fallback"), not a defeat of the correction.
    ///
    /// See `dag::propagate_failure_forced_terminal`'s doc comment for the full rationale on
    /// why it is safe to skip recovery/reroute/resurrection entirely.
    ///
    /// # Why this is safe (no `RegionHealth` double-counting)
    ///
    /// Neither `dag::propagate_failure` nor `dag::propagate_failure_forced_terminal` touches
    /// `cascade_detector`/`RegionHealth` — that bookkeeping lives exclusively in
    /// `handle_completed_outcome` and `handle_failed_outcome`, neither of which this method
    /// calls. This task was already recorded as a success by `handle_completed_outcome`
    /// before the correction; this method only cancels/skips dependents and updates
    /// `GraphStatus`, exactly the consequences the original #6380 fix deferred to avoid
    /// double-recording this task itself. The special-cases above are what make this claim
    /// actually hold for every `FailureStrategy` (and every `recovery` configuration) —
    /// routing `Retry`/`Ask`, or `Abort`-with-`state_injection`, through unmodified
    /// `propagate_failure` would have made the claim false (a redispatch re-invokes
    /// `record_outcome`; `try_recover` un-fails the task entirely), which is exactly what the
    /// special-cases prevent.
    ///
    /// # Remaining limitation
    ///
    /// Dependents that already reached a terminal status (most commonly `Completed`, having
    /// already consumed this task's now-invalidated output) before this correction landed
    /// are **not** retroactively unwound — neither propagation function's cancellation/skip
    /// logic affects dependents already past `Pending`/`Ready`/`Running`. This mirrors the
    /// `RunInline` path's own limitation (a `Completed` dependent is never revisited there
    /// either) and is intentionally out of scope here — fully unwinding already-finished
    /// downstream work would require re-running or invalidating those tasks, a materially
    /// larger change than restoring propagation parity.
    pub fn propagate_corrected_task_failure(&mut self, task_id: TaskId) -> Vec<SchedulerAction> {
        let task = &self.graph.tasks[task_id.index()];
        let effective_strategy = task
            .failure_strategy
            .unwrap_or(self.graph.default_failure_strategy);
        let has_state_injection = task
            .recovery
            .as_ref()
            .and_then(|r| r.state_injection.as_ref())
            .is_some();
        // #6396/S1-residual (critic, 2026-07-17): `Retry`/`Ask` always need the forced-terminal
        // path (see doc comment above). `Abort`'s own arm in `dag::propagate_failure` also
        // calls `try_recover` *before* terminal-failing the graph — when `state_injection` is
        // configured, that would flip this just-corrected task straight back to `Completed`,
        // silently undoing the correction exactly like the Retry/Ask hazard this method already
        // guards against. `Skip` never calls `try_recover` (its arm goes straight to
        // `skip_subtree`), so it is excluded regardless of `state_injection`.
        let needs_forced_terminal =
            matches!(
                effective_strategy,
                zeph_config::FailureStrategy::Retry | zeph_config::FailureStrategy::Ask
            ) || (effective_strategy == zeph_config::FailureStrategy::Abort && has_state_injection);
        let cancel_ids = if needs_forced_terminal {
            dag::propagate_failure_forced_terminal(&mut self.graph, task_id)
        } else {
            dag::propagate_failure(&mut self.graph, task_id, &self.topology.rev_adj)
        };
        let mut actions = Vec::new();

        for cancel_task_id in cancel_ids {
            if let Some(running) = self.running.remove(&cancel_task_id) {
                actions.push(SchedulerAction::Cancel {
                    agent_handle_id: running.agent_handle_id,
                });
            }
        }

        if self.graph.status != GraphStatus::Running {
            self.graph.finished_at = Some(crate::graph::chrono_now());
            actions.push(SchedulerAction::Done {
                status: self.graph.status,
            });
        }

        actions
    }

    /// Apply the Failed outcome branch: build lineage, evaluate cascade abort, propagate failure.
    fn handle_failed_outcome(&mut self, task_id: TaskId, error: &str) -> Vec<SchedulerAction> {
        self.graph_dirty = true;
        // SEC-ORCH-04: truncate error to avoid logging sensitive internal details.
        let error_excerpt: String = error.chars().take(512).collect();
        tracing::warn!(
            task_id = %task_id,
            error = %error_excerpt,
            "task failed"
        );
        self.graph.tasks[task_id.index()].status = TaskStatus::Failed;
        // Populate `.result` with the failure error so Mode-2 `routed_from` prompt injection
        // (router.rs's `build_task_prompt`) has real content to surface, and so
        // `finalize_plan_failed`'s error-message formatting doesn't fall back to
        // "unknown error". `agent_id`/`agent_def`/`duration_ms` are left at their zero
        // values -- this is failure diagnostics, not a completed-task provenance record.
        self.graph.tasks[task_id.index()].result = Some(TaskResult {
            output: error_excerpt,
            artifacts: Vec::new(),
            duration_ms: 0,
            agent_id: None,
            agent_def: None,
        });

        if let Some(ref mut detector) = self.cascade_detector {
            detector.record_outcome(task_id, false, &self.graph);
        }

        // Build error lineage chain from parent chains (S4 side-table).
        // BEFORE propagate_failure so we can read the graph topology.
        let deps: Vec<TaskId> = self.graph.tasks[task_id.index()].depends_on.clone();
        let mut chain = ErrorLineage::default();
        for parent_id in &deps {
            if let Some(parent_chain) = self.lineage_chains.get(parent_id) {
                chain.merge(parent_chain, self.lineage_ttl_secs);
            }
        }
        chain.push(LineageEntry {
            task_id,
            kind: LineageKind::Failed {
                error_class: classify_error(error),
            },
            ts_ms: now_ms(),
        });
        self.lineage_chains.insert(task_id, chain.clone());

        let ttl = self.lineage_ttl_secs;
        self.lineage_chains.retain(|_, c| c.is_recent(ttl));

        // Check fan-out abort signal from CascadeDetector.
        let graph = &self.graph;
        let threshold = self.cascade_failure_rate_abort_threshold;
        if let Some(ref mut detector) = self.cascade_detector {
            match detector.evaluate_abort(graph, task_id, threshold) {
                crate::cascade::AbortDecision::FanOutCascade {
                    region_root,
                    failure_rate,
                    region_size,
                } => {
                    tracing::error!(
                        root = %region_root,
                        failure_rate = failure_rate,
                        region_size = region_size,
                        cause = "fan_out_rate",
                        "cascade abort: fan-out failure rate threshold exceeded"
                    );
                    return self.abort_dag_with_lineage(region_root, chain.entries());
                }
                crate::cascade::AbortDecision::None => {}
            }
        }

        // Check linear-chain abort signal (consecutive failures in depends_on path).
        if self.cascade_chain_threshold > 0
            && chain.consecutive_failed_len() >= self.cascade_chain_threshold
        {
            let root_id = chain.first_entry().map_or(task_id, |e| e.task_id);
            tracing::error!(
                root = %root_id,
                chain_depth = chain.consecutive_failed_len(),
                threshold = self.cascade_chain_threshold,
                cause = "chain_threshold",
                "cascade abort: consecutive failure chain threshold exceeded"
            );
            return self.abort_dag_with_lineage(root_id, chain.entries());
        }

        let cancel_ids = dag::propagate_failure(&mut self.graph, task_id, &self.topology.rev_adj);
        let mut actions = Vec::new();

        for cancel_task_id in cancel_ids {
            if let Some(running) = self.running.remove(&cancel_task_id) {
                actions.push(SchedulerAction::Cancel {
                    agent_handle_id: running.agent_handle_id,
                });
            }
        }

        if self.graph.status != GraphStatus::Running {
            self.graph.finished_at = Some(crate::graph::chrono_now());
            actions.push(SchedulerAction::Done {
                status: self.graph.status,
            });
        }

        actions
    }

    /// Abort the DAG due to cascade failure; cancel all running tasks.
    ///
    /// Sets graph status to `Failed`, records `finished_at`, emits `Cancel` for all
    /// running tasks, and appends `Done`. The `chain` is logged here for the audit record.
    /// Callers must emit `tracing::error!` with root/cause before calling this.
    fn abort_dag_with_lineage(
        &mut self,
        root: TaskId,
        chain: &[crate::lineage::LineageEntry],
    ) -> Vec<SchedulerAction> {
        self.graph.status = GraphStatus::Failed;
        self.graph.finished_at = Some(crate::graph::chrono_now());

        // Emit structured audit log entry with full lineage path.
        tracing::error!(
            root = %root,
            chain_depth = chain.len(),
            chain = ?chain.iter().map(|e| e.task_id).collect::<Vec<_>>(),
            "cascade abort: DAG terminated"
        );

        let mut actions: Vec<SchedulerAction> = self
            .running
            .drain()
            .map(|(_, r)| SchedulerAction::Cancel {
                agent_handle_id: r.agent_handle_id,
            })
            .collect();

        actions.push(SchedulerAction::Done {
            status: self.graph.status,
        });
        actions
    }

    /// Compute the effective run-timeout for a task: its per-task
    /// `TaskNode.timeout.run_timeout_secs` override when set, else the graph-global
    /// `self.task_timeout` default.
    fn effective_run_timeout(&self, task_id: TaskId) -> Duration {
        self.graph.tasks[task_id.index()]
            .timeout
            .as_ref()
            .and_then(|t| t.run_timeout_secs)
            .map_or(self.task_timeout, Duration::from_secs)
    }

    /// Compute the effective idle-timeout for a task: its per-task
    /// `TaskNode.timeout.idle_timeout_secs` override when set, else the graph-global
    /// `self.default_idle_timeout` default. Returns `None` when neither is set — idle
    /// enforcement is opt-in, unlike `effective_run_timeout` which always has a value.
    fn effective_idle_timeout(&self, task_id: TaskId) -> Option<Duration> {
        self.graph.tasks[task_id.index()]
            .timeout
            .as_ref()
            .and_then(|t| t.idle_timeout_secs)
            .map(Duration::from_secs)
            .or(self.default_idle_timeout)
    }

    /// Check all running tasks for timeout violations.
    ///
    /// Evaluates run-timeout before idle-timeout for each task, so a task that has
    /// exceeded both on the same tick is reported with a single cause: run wins (it is the
    /// hard wall-clock cap; idle is the softer no-progress liveness signal). Satisfies
    /// NFR-OB-01 (spec-075 §"timeout-cause disambiguation") — the log line and the
    /// synthetic `TaskResult.output` below always name exactly one mechanism.
    ///
    /// Idle detection reads `RunningTask::last_progress_at` with `Ordering::Relaxed`. A
    /// stale `Relaxed` load can only observe an *older* heartbeat timestamp, which biases
    /// toward firing *earlier* than the true idle time — never later — so this can never
    /// mask a genuinely idle task. The false-positive risk from that bias is vanishingly
    /// small in practice: staleness is bounded by cache-coherence propagation (sub-µs on
    /// tier-1 hardware) versus a threshold measured in seconds-to-minutes, and every tick
    /// re-`load`s a fresh value, so a value missed on one tick is observed well before a
    /// second wrongful crossing on the next. `Acquire`/`Release` would remove even that
    /// vanishing risk but is unjustified overhead for a cooperative liveness timeout.
    ///
    /// # Warning: Cooperative Cancellation
    ///
    /// Cancel actions emitted here signal agents cooperatively. Tool operations in progress
    /// at the time of cancellation complete before the agent loop checks the cancellation
    /// token. Partially-written artifacts may remain on disk after cancellation.
    fn check_timeouts(&mut self) -> Vec<SchedulerAction> {
        let timed_out: Vec<(TaskId, String, TimeoutCause, Duration)> = self
            .running
            .iter()
            .filter_map(|(id, r)| {
                let run_limit = self.effective_run_timeout(*id);
                if r.started_at.elapsed() > run_limit {
                    return Some((*id, r.agent_handle_id.clone(), TimeoutCause::Run, run_limit));
                }

                let idle_limit = self.effective_idle_timeout(*id)?;
                let progress = r.last_progress_at.as_ref()?;
                let idle_elapsed_ms = zeph_common::monotonic_millis()
                    .saturating_sub(progress.load(Ordering::Relaxed));
                let idle_limit_ms = u64::try_from(idle_limit.as_millis()).unwrap_or(u64::MAX);
                (idle_elapsed_ms > idle_limit_ms).then(|| {
                    (
                        *id,
                        r.agent_handle_id.clone(),
                        TimeoutCause::Idle,
                        idle_limit,
                    )
                })
            })
            .collect();

        let mut actions = Vec::new();
        for (task_id, agent_handle_id, cause, limit) in timed_out {
            tracing::warn!(
                task_id = %task_id,
                cause = ?cause,
                timeout_secs = limit.as_secs(),
                "task timed out"
            );
            self.graph_dirty = true;

            let removed = self.running.remove(&task_id);
            self.graph.tasks[task_id.index()].status = TaskStatus::Failed;
            // `.result` is populated below (cause-aware) so Mode-2 `routed_from` prompt
            // injection (router.rs's `build_task_prompt`) has real content to surface, and so
            // `finalize_plan_failed`'s error-message formatting doesn't fall back to
            // "unknown error".

            let duration_ms = removed.as_ref().map_or(0, |r| {
                u64::try_from(r.started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
            });
            let agent_def_name = removed.map(|r| r.agent_def_name);
            self.graph.tasks[task_id.index()].result = Some(TaskResult {
                output: match cause {
                    TimeoutCause::Run => {
                        format!("task exceeded run timeout ({}s)", limit.as_secs())
                    }
                    TimeoutCause::Idle => format!(
                        "task exceeded idle timeout ({}s of no progress)",
                        limit.as_secs()
                    ),
                },
                artifacts: Vec::new(),
                duration_ms,
                agent_id: Some(agent_handle_id.clone()),
                agent_def: agent_def_name,
            });

            actions.push(SchedulerAction::Cancel { agent_handle_id });

            let cancel_ids =
                dag::propagate_failure(&mut self.graph, task_id, &self.topology.rev_adj);
            for cancel_task_id in cancel_ids {
                if let Some(running) = self.running.remove(&cancel_task_id) {
                    actions.push(SchedulerAction::Cancel {
                        agent_handle_id: running.agent_handle_id,
                    });
                }
            }

            if self.graph.status != GraphStatus::Running {
                self.graph.finished_at = Some(crate::graph::chrono_now());
                actions.push(SchedulerAction::Done {
                    status: self.graph.status,
                });
                break;
            }
        }

        actions
    }
}

/// Which mechanism fired a task timeout — carried through `check_timeouts` so the log line
/// and the synthetic `TaskResult.output` always name a single cause (NFR-OB-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeoutCause {
    /// Hard wall-clock cap exceeded (`effective_run_timeout`). Always enforced.
    Run,
    /// No progress heartbeat observed within the idle window (`effective_idle_timeout`).
    /// Opt-in: only fires when an idle limit is configured AND the task carries a live
    /// progress handle (`RunningTask::last_progress_at.is_some()`).
    Idle,
}

#[cfg(test)]
mod tests;
