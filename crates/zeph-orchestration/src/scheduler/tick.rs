// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Core tick/execution loop, event processing, and spawn record-keeping.

use std::time::Duration;

use super::{DagScheduler, RunningTask, SchedulerAction, TaskEvent, TaskOutcome};
use crate::dag;
use crate::graph::{ExecutionMode, GraphStatus, TaskId, TaskResult, TaskStatus};
use crate::lineage::{ErrorLineage, LineageEntry, LineageKind, classify_error, now_ms};
use crate::topology::DispatchStrategy;
use zeph_subagent::SubAgentError;

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

            // LevelBarrier: only dispatch tasks at the current level.
            if self.topology.strategy == DispatchStrategy::LevelBarrier {
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

            let task = &self.graph.tasks[task_id.index()];

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
    pub async fn wait_event(&mut self) {
        if self.running.is_empty() {
            tokio::time::sleep(self.current_deferral_backoff()).await;
            return;
        }

        // Find the nearest timeout deadline among running tasks.
        let nearest_timeout = self
            .running
            .values()
            .map(|r| {
                self.task_timeout
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
    pub fn record_spawn(
        &mut self,
        task_id: TaskId,
        agent_handle_id: String,
        agent_def_name: String,
    ) {
        self.consecutive_spawn_failures = 0;
        self.graph.tasks[task_id.index()].assigned_agent = Some(agent_handle_id.clone());
        self.running.insert(
            task_id,
            RunningTask {
                agent_handle_id,
                agent_def_name,
                started_at: std::time::Instant::now(),
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
        self.graph.tasks[task_id.index()].status = TaskStatus::Failed;
        let cancel_ids = dag::propagate_failure(&mut self.graph, task_id);
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
            TaskOutcome::Completed { output, artifacts } => self.handle_completed_outcome(
                task_id,
                agent_handle_id,
                agent_def_name,
                duration_ms,
                output,
                artifacts,
            ),
            TaskOutcome::Failed { error } => self.handle_failed_outcome(task_id, &error),
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
        output: String,
        artifacts: Vec<std::path::PathBuf>,
    ) -> Vec<SchedulerAction> {
        self.graph.tasks[task_id.index()].status = TaskStatus::Completed;
        self.graph.tasks[task_id.index()].result = Some(TaskResult {
            output: output.clone(),
            artifacts,
            duration_ms,
            agent_id: Some(agent_handle_id),
            agent_def: agent_def_name,
        });

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

        // Emit Verify action when verify_completeness is enabled.
        // The replan budget is enforced inside inject_tasks() — the observation
        // (emitting Verify) must not be gated on the mutation budget, or tasks
        // after budget exhaustion never receive verification at all.
        // max_replans=0 still emits Verify; gaps are logged only (no inject_tasks call).
        if self.verify_completeness {
            vec![SchedulerAction::Verify { task_id, output }]
        } else {
            Vec::new()
        }
    }

    /// Apply the Failed outcome branch: build lineage, evaluate cascade abort, propagate failure.
    fn handle_failed_outcome(&mut self, task_id: TaskId, error: &str) -> Vec<SchedulerAction> {
        // SEC-ORCH-04: truncate error to avoid logging sensitive internal details.
        let error_excerpt: String = error.chars().take(512).collect();
        tracing::warn!(
            task_id = %task_id,
            error = %error_excerpt,
            "task failed"
        );
        self.graph.tasks[task_id.index()].status = TaskStatus::Failed;

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

        let cancel_ids = dag::propagate_failure(&mut self.graph, task_id);
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

    /// Check all running tasks for timeout violations.
    ///
    /// # Warning: Cooperative Cancellation
    ///
    /// Cancel actions emitted here signal agents cooperatively. Tool operations in progress
    /// at the time of cancellation complete before the agent loop checks the cancellation
    /// token. Partially-written artifacts may remain on disk after cancellation.
    fn check_timeouts(&mut self) -> Vec<SchedulerAction> {
        let timed_out: Vec<(TaskId, String)> = self
            .running
            .iter()
            .filter(|(_, r)| r.started_at.elapsed() > self.task_timeout)
            .map(|(id, r)| (*id, r.agent_handle_id.clone()))
            .collect();

        let mut actions = Vec::new();
        for (task_id, agent_handle_id) in timed_out {
            tracing::warn!(
                task_id = %task_id,
                timeout_secs = self.task_timeout.as_secs(),
                "task timed out"
            );
            self.running.remove(&task_id);
            self.graph.tasks[task_id.index()].status = TaskStatus::Failed;

            actions.push(SchedulerAction::Cancel { agent_handle_id });

            let cancel_ids = dag::propagate_failure(&mut self.graph, task_id);
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::scheduler::tests::*;

    #[test]
    fn test_tick_produces_spawn_for_ready() {
        let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
        let mut scheduler = make_scheduler(graph);
        let actions = scheduler.tick();
        let spawns: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
            .collect();
        assert_eq!(spawns.len(), 2);
    }

    #[test]
    fn test_tick_dispatches_all_regardless_of_max_parallel() {
        // tick() enforces max_parallel as a pre-dispatch cap.
        // With 5 independent tasks and max_parallel=2, only 2 are dispatched per tick.
        let graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[]),
            make_node(3, &[]),
            make_node(4, &[]),
        ]);
        let mut config = make_config();
        config.max_parallel = 2;
        let defs = vec![make_def("worker")];
        let mut scheduler = DagScheduler::new(graph, &config, Box::new(FirstRouter), defs).unwrap();
        let actions = scheduler.tick();
        let spawn_count = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
            .count();
        assert_eq!(
            spawn_count, 2,
            "max_parallel=2 caps dispatched tasks per tick"
        );
    }

    #[test]
    fn test_tick_detects_completion() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Completed;
        let config = make_config();
        let defs = vec![make_def("worker")];
        let mut scheduler = DagScheduler::new(graph, &config, Box::new(FirstRouter), defs).unwrap();
        // Manually set graph to Running since new() validated Created status
        // — but all tasks are terminal. tick() should detect completion.
        let actions = scheduler.tick();
        let has_done = actions.iter().any(|a| {
            matches!(
                a,
                SchedulerAction::Done {
                    status: GraphStatus::Completed
                }
            )
        });
        assert!(
            has_done,
            "should emit Done(Completed) when all tasks are terminal"
        );
    }

    #[test]
    fn test_completion_event_marks_deps_ready() {
        let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        let mut scheduler = make_scheduler(graph);

        // Simulate task 0 running.
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "handle-0".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: std::time::Instant::now(),
            },
        );

        let event = TaskEvent {
            task_id: TaskId(0),
            agent_handle_id: "handle-0".to_string(),
            outcome: TaskOutcome::Completed {
                output: "done".to_string(),
                artifacts: vec![],
            },
        };
        scheduler.buffered_events.push_back(event);

        let actions = scheduler.tick();
        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Completed);
        // Task 1 should now be Ready or Spawn action emitted.
        let has_spawn_1 = actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::Spawn { task_id, .. } if *task_id == TaskId(1)));
        assert!(
            has_spawn_1 || scheduler.graph.tasks[1].status == TaskStatus::Ready,
            "task 1 should be spawned or marked Ready"
        );
    }

    #[test]
    fn test_failure_abort_cancels_running() {
        let graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[0, 1]),
        ]);
        let mut scheduler = make_scheduler(graph);

        // Simulate tasks 0 and 1 running.
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "h0".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: std::time::Instant::now(),
            },
        );
        scheduler.graph.tasks[1].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(1),
            RunningTask {
                agent_handle_id: "h1".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: std::time::Instant::now(),
            },
        );

        // Task 0 fails with default Abort strategy.
        let event = TaskEvent {
            task_id: TaskId(0),
            agent_handle_id: "h0".to_string(),
            outcome: TaskOutcome::Failed {
                error: "boom".to_string(),
            },
        };
        scheduler.buffered_events.push_back(event);

        let actions = scheduler.tick();
        assert_eq!(scheduler.graph.status, GraphStatus::Failed);
        let cancel_ids: Vec<_> = actions
            .iter()
            .filter_map(|a| {
                if let SchedulerAction::Cancel { agent_handle_id } = a {
                    Some(agent_handle_id.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert!(cancel_ids.contains(&"h1"), "task 1 should be canceled");
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, SchedulerAction::Done { .. }))
        );
    }

    #[test]
    fn test_failure_skip_propagates() {
        use crate::graph::FailureStrategy;

        let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        let mut scheduler = make_scheduler(graph);

        // Set failure strategy to Skip on task 0.
        scheduler.graph.tasks[0].failure_strategy = Some(FailureStrategy::Skip);
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "h0".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: std::time::Instant::now(),
            },
        );

        let event = TaskEvent {
            task_id: TaskId(0),
            agent_handle_id: "h0".to_string(),
            outcome: TaskOutcome::Failed {
                error: "skip me".to_string(),
            },
        };
        scheduler.buffered_events.push_back(event);
        scheduler.tick();

        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Skipped);
        assert_eq!(scheduler.graph.tasks[1].status, TaskStatus::Skipped);
    }

    #[test]
    fn test_failure_retry_reschedules() {
        use crate::graph::FailureStrategy;

        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);

        scheduler.graph.tasks[0].failure_strategy = Some(FailureStrategy::Retry);
        scheduler.graph.tasks[0].max_retries = Some(3);
        scheduler.graph.tasks[0].retry_count = 0;
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "h0".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: std::time::Instant::now(),
            },
        );

        let event = TaskEvent {
            task_id: TaskId(0),
            agent_handle_id: "h0".to_string(),
            outcome: TaskOutcome::Failed {
                error: "transient".to_string(),
            },
        };
        scheduler.buffered_events.push_back(event);
        let actions = scheduler.tick();

        // Task should be rescheduled (Ready) and a Spawn action emitted.
        let has_spawn = actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::Spawn { task_id, .. } if *task_id == TaskId(0)));
        assert!(
            has_spawn || scheduler.graph.tasks[0].status == TaskStatus::Ready,
            "retry should produce spawn or Ready status"
        );
        // retry_count incremented
        assert_eq!(scheduler.graph.tasks[0].retry_count, 1);
    }

    #[test]
    fn test_process_event_failed_retry() {
        use crate::graph::FailureStrategy;

        // End-to-end: send Failed event, verify retry path produces Ready -> Spawn.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);

        scheduler.graph.tasks[0].failure_strategy = Some(FailureStrategy::Retry);
        scheduler.graph.tasks[0].max_retries = Some(2);
        scheduler.graph.tasks[0].retry_count = 0;
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "h0".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: std::time::Instant::now(),
            },
        );

        let event = TaskEvent {
            task_id: TaskId(0),
            agent_handle_id: "h0".to_string(),
            outcome: TaskOutcome::Failed {
                error: "first failure".to_string(),
            },
        };
        scheduler.buffered_events.push_back(event);
        let actions = scheduler.tick();

        // After retry: retry_count = 1, status = Ready or Spawn emitted.
        assert_eq!(scheduler.graph.tasks[0].retry_count, 1);
        let spawned = actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::Spawn { task_id, .. } if *task_id == TaskId(0)));
        assert!(
            spawned || scheduler.graph.tasks[0].status == TaskStatus::Ready,
            "retry should emit Spawn or set Ready"
        );
        // Graph must still be Running.
        assert_eq!(scheduler.graph.status, GraphStatus::Running);
    }

    #[test]
    fn test_timeout_cancels_stalled() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut config = make_config();
        config.task_timeout_secs = 1; // 1 second timeout
        let defs = vec![make_def("worker")];
        let mut scheduler = DagScheduler::new(graph, &config, Box::new(FirstRouter), defs).unwrap();

        // Simulate a running task that started just over 1 second ago.
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "h0".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: std::time::Instant::now()
                    .checked_sub(Duration::from_secs(2))
                    .unwrap(), // already timed out
            },
        );

        let actions = scheduler.tick();
        let has_cancel = actions.iter().any(
            |a| matches!(a, SchedulerAction::Cancel { agent_handle_id } if agent_handle_id == "h0"),
        );
        assert!(has_cancel, "timed-out task should emit Cancel action");
        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Failed);
    }

    #[test]
    fn test_cancel_all() {
        let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
        let mut scheduler = make_scheduler(graph);

        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "h0".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: std::time::Instant::now(),
            },
        );
        scheduler.graph.tasks[1].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(1),
            RunningTask {
                agent_handle_id: "h1".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: std::time::Instant::now(),
            },
        );

        let actions = scheduler.cancel_all();

        assert_eq!(scheduler.graph.status, GraphStatus::Canceled);
        assert!(scheduler.running.is_empty());
        let cancel_count = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::Cancel { .. }))
            .count();
        assert_eq!(cancel_count, 2);
        assert!(actions.iter().any(|a| matches!(
            a,
            SchedulerAction::Done {
                status: GraphStatus::Canceled
            }
        )));
    }

    #[test]
    fn test_record_spawn_failure() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);

        // Simulate task marked Running (by tick) but spawn failed.
        scheduler.graph.tasks[0].status = TaskStatus::Running;

        let error = SubAgentError::Spawn("spawn error".to_string());
        let actions = scheduler.record_spawn_failure(TaskId(0), &error);
        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Failed);
        // With Abort strategy and no other running tasks, graph should be Failed.
        assert_eq!(scheduler.graph.status, GraphStatus::Failed);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, SchedulerAction::Done { .. }))
        );
    }

    #[test]
    fn test_record_spawn_failure_concurrency_limit_reverts_to_ready() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);

        // Simulate tick() optimistically marking the task Running before spawn.
        scheduler.graph.tasks[0].status = TaskStatus::Running;

        // Concurrency limit hit — transient, should not fail the task.
        let error = SubAgentError::ConcurrencyLimit { active: 4, max: 4 };
        let actions = scheduler.record_spawn_failure(TaskId(0), &error);
        assert_eq!(
            scheduler.graph.tasks[0].status,
            TaskStatus::Ready,
            "task must revert to Ready so the next tick can retry"
        );
        assert_eq!(
            scheduler.graph.status,
            GraphStatus::Running,
            "graph must stay Running, not transition to Failed"
        );
        assert!(
            actions.is_empty(),
            "no cancel or done actions expected for a transient deferral"
        );
    }

    #[test]
    fn test_record_spawn_failure_concurrency_limit_variant_spawn_for_task() {
        // Both spawn() and resume() now return SubAgentError::ConcurrencyLimit — verify handling.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);
        scheduler.graph.tasks[0].status = TaskStatus::Running;

        let error = SubAgentError::ConcurrencyLimit { active: 1, max: 1 };
        let actions = scheduler.record_spawn_failure(TaskId(0), &error);
        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Ready);
        assert!(actions.is_empty());
    }

    #[test]
    fn test_concurrency_deferral_does_not_affect_running_task() {
        let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
        let mut scheduler = make_scheduler(graph);

        // Simulate both tasks optimistically marked Running by tick().
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "h0".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: std::time::Instant::now(),
            },
        );
        scheduler.graph.tasks[1].status = TaskStatus::Running;

        // Task 1 spawn fails with concurrency limit.
        let error = SubAgentError::ConcurrencyLimit { active: 1, max: 1 };
        let actions = scheduler.record_spawn_failure(TaskId(1), &error);

        assert_eq!(
            scheduler.graph.tasks[0].status,
            TaskStatus::Running,
            "task 0 must remain Running"
        );
        assert_eq!(
            scheduler.graph.tasks[1].status,
            TaskStatus::Ready,
            "task 1 must revert to Ready"
        );
        assert_eq!(
            scheduler.graph.status,
            GraphStatus::Running,
            "graph must stay Running"
        );
        assert!(actions.is_empty(), "no cancel or done actions expected");
    }

    #[test]
    fn test_max_concurrent_zero_no_infinite_loop() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let config = zeph_config::OrchestrationConfig {
            max_parallel: 0,
            ..make_config()
        };
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        let actions1 = scheduler.tick();
        assert!(
            actions1
                .iter()
                .all(|a| !matches!(a, SchedulerAction::Spawn { .. })),
            "no Spawn expected when max_parallel=0"
        );
        assert!(
            actions1
                .iter()
                .all(|a| !matches!(a, SchedulerAction::Done { .. })),
            "no Done(Failed) expected — ready tasks exist, so no deadlock"
        );
        assert_eq!(scheduler.graph.status, GraphStatus::Running);

        let actions2 = scheduler.tick();
        assert!(
            actions2
                .iter()
                .all(|a| !matches!(a, SchedulerAction::Done { .. })),
            "second tick must not emit Done(Failed) — ready tasks still exist"
        );
        assert_eq!(
            scheduler.graph.status,
            GraphStatus::Running,
            "graph must remain Running"
        );
    }

    #[test]
    fn test_all_tasks_deferred_graph_stays_running() {
        let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
        let mut scheduler = make_scheduler(graph);

        // First tick emits Spawn for both tasks and marks them Running.
        let actions = scheduler.tick();
        assert_eq!(
            actions
                .iter()
                .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
                .count(),
            2,
            "expected 2 Spawn actions on first tick"
        );
        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Running);
        assert_eq!(scheduler.graph.tasks[1].status, TaskStatus::Running);

        // Both spawns fail — both revert to Ready.
        let error = SubAgentError::ConcurrencyLimit { active: 2, max: 2 };
        let r0 = scheduler.record_spawn_failure(TaskId(0), &error);
        let r1 = scheduler.record_spawn_failure(TaskId(1), &error);
        assert!(r0.is_empty() && r1.is_empty(), "no cancel/done on deferral");
        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Ready);
        assert_eq!(scheduler.graph.tasks[1].status, TaskStatus::Ready);
        assert_eq!(scheduler.graph.status, GraphStatus::Running);

        // Second tick must retry both deferred tasks.
        let retry_actions = scheduler.tick();
        let spawn_count = retry_actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
            .count();
        assert!(
            spawn_count > 0,
            "second tick must re-emit Spawn for deferred tasks"
        );
        assert!(
            retry_actions.iter().all(|a| !matches!(
                a,
                SchedulerAction::Done {
                    status: GraphStatus::Failed,
                    ..
                }
            )),
            "no Done(Failed) expected"
        );
    }

    #[test]
    fn test_no_agent_routes_inline() {
        // NoneRouter: when no agent matches, task falls back to RunInline.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler_with_router(graph, Box::new(NoneRouter));
        let actions = scheduler.tick();
        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Running);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, SchedulerAction::RunInline { .. }))
        );
    }

    #[test]
    fn test_stale_event_rejected() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);

        // Simulate task running with handle "current-handle".
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "current-handle".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: std::time::Instant::now(),
            },
        );

        // Send a completion event from the OLD agent (stale handle).
        let stale_event = TaskEvent {
            task_id: TaskId(0),
            agent_handle_id: "old-handle".to_string(),
            outcome: TaskOutcome::Completed {
                output: "stale output".to_string(),
                artifacts: vec![],
            },
        };
        scheduler.buffered_events.push_back(stale_event);
        let actions = scheduler.tick();

        assert_ne!(
            scheduler.graph.tasks[0].status,
            TaskStatus::Completed,
            "stale event must not complete the task"
        );
        let has_done = actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::Done { .. }));
        assert!(
            !has_done,
            "no Done action should be emitted for a stale event"
        );
        assert!(
            scheduler.running.contains_key(&TaskId(0)),
            "running task must remain after stale event"
        );
    }

    #[test]
    fn test_duration_ms_computed_correctly() {
        // Regression test for C1: duration_ms must be non-zero after completion.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);

        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "h0".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: std::time::Instant::now()
                    .checked_sub(Duration::from_millis(50))
                    .unwrap(),
            },
        );

        let event = TaskEvent {
            task_id: TaskId(0),
            agent_handle_id: "h0".to_string(),
            outcome: TaskOutcome::Completed {
                output: "result".to_string(),
                artifacts: vec![],
            },
        };
        scheduler.buffered_events.push_back(event);
        scheduler.tick();

        let result = scheduler.graph.tasks[0].result.as_ref().unwrap();
        assert!(
            result.duration_ms > 0,
            "duration_ms should be > 0, got {}",
            result.duration_ms
        );
    }

    // --- #1619 regression tests: consecutive_spawn_failures + exponential backoff ---

    #[test]
    fn test_consecutive_spawn_failures_increments_on_concurrency_limit() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);
        scheduler.graph.tasks[0].status = TaskStatus::Running;

        assert_eq!(scheduler.consecutive_spawn_failures, 0, "starts at zero");

        let error = SubAgentError::ConcurrencyLimit { active: 4, max: 4 };
        scheduler.record_spawn_failure(TaskId(0), &error);
        scheduler.record_batch_backoff(false, true);
        assert_eq!(
            scheduler.consecutive_spawn_failures, 1,
            "first deferral tick: consecutive_spawn_failures must be 1"
        );

        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.record_spawn_failure(TaskId(0), &error);
        scheduler.record_batch_backoff(false, true);
        assert_eq!(
            scheduler.consecutive_spawn_failures, 2,
            "second deferral tick: consecutive_spawn_failures must be 2"
        );

        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.record_spawn_failure(TaskId(0), &error);
        scheduler.record_batch_backoff(false, true);
        assert_eq!(
            scheduler.consecutive_spawn_failures, 3,
            "third deferral tick: consecutive_spawn_failures must be 3"
        );
    }

    #[test]
    fn test_consecutive_spawn_failures_resets_on_success() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);
        scheduler.graph.tasks[0].status = TaskStatus::Running;

        let error = SubAgentError::ConcurrencyLimit { active: 1, max: 1 };
        scheduler.record_spawn_failure(TaskId(0), &error);
        scheduler.record_batch_backoff(false, true);
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.record_spawn_failure(TaskId(0), &error);
        scheduler.record_batch_backoff(false, true);
        assert_eq!(scheduler.consecutive_spawn_failures, 2);

        scheduler.record_spawn(TaskId(0), "handle-0".to_string(), "worker".to_string());
        assert_eq!(
            scheduler.consecutive_spawn_failures, 0,
            "record_spawn must reset consecutive_spawn_failures to 0"
        );
    }

    #[tokio::test]
    async fn test_exponential_backoff_duration() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let config = zeph_config::OrchestrationConfig {
            deferral_backoff_ms: 50,
            ..make_config()
        };
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        // consecutive_spawn_failures=0 → sleep ≈ 50ms (base).
        assert_eq!(scheduler.consecutive_spawn_failures, 0);
        let start = tokio::time::Instant::now();
        scheduler.wait_event().await;
        let elapsed0 = start.elapsed();
        assert!(
            elapsed0.as_millis() >= 50,
            "backoff with 0 deferrals must be >= base (50ms), got {}ms",
            elapsed0.as_millis()
        );

        // Simulate 3 consecutive deferrals: multiplier = 2^3 = 8 → 400ms, capped at 5000ms.
        scheduler.consecutive_spawn_failures = 3;
        let start = tokio::time::Instant::now();
        scheduler.wait_event().await;
        let elapsed3 = start.elapsed();
        assert!(
            elapsed3.as_millis() >= 400,
            "backoff with 3 deferrals must be >= 400ms (50 * 8), got {}ms",
            elapsed3.as_millis()
        );

        // Simulate 20 consecutive deferrals: exponent capped at 10 → 50 * 1024 = 51200 → capped at 5000ms.
        scheduler.consecutive_spawn_failures = 20;
        let start = tokio::time::Instant::now();
        scheduler.wait_event().await;
        let elapsed_capped = start.elapsed();
        assert!(
            elapsed_capped.as_millis() >= 5000,
            "backoff must be capped at 5000ms with high deferrals, got {}ms",
            elapsed_capped.as_millis()
        );
    }

    #[tokio::test]
    async fn test_wait_event_sleeps_deferral_backoff_when_running_empty() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let config = zeph_config::OrchestrationConfig {
            deferral_backoff_ms: 50,
            ..make_config()
        };
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        assert!(scheduler.running.is_empty());

        let start = tokio::time::Instant::now();
        scheduler.wait_event().await;
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_millis() >= 50,
            "wait_event must sleep at least deferral_backoff (50ms) when running is empty, but only slept {}ms",
            elapsed.as_millis()
        );
    }

    #[test]
    fn test_current_deferral_backoff_exponential_growth() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let config = zeph_config::OrchestrationConfig {
            deferral_backoff_ms: 250,
            ..make_config()
        };
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        assert_eq!(
            scheduler.current_deferral_backoff(),
            Duration::from_millis(250)
        );

        scheduler.consecutive_spawn_failures = 1;
        assert_eq!(
            scheduler.current_deferral_backoff(),
            Duration::from_millis(500)
        );

        scheduler.consecutive_spawn_failures = 2;
        assert_eq!(scheduler.current_deferral_backoff(), Duration::from_secs(1));

        scheduler.consecutive_spawn_failures = 3;
        assert_eq!(scheduler.current_deferral_backoff(), Duration::from_secs(2));

        scheduler.consecutive_spawn_failures = 4;
        assert_eq!(scheduler.current_deferral_backoff(), Duration::from_secs(4));

        // Cap at 5 seconds.
        scheduler.consecutive_spawn_failures = 5;
        assert_eq!(scheduler.current_deferral_backoff(), Duration::from_secs(5));

        scheduler.consecutive_spawn_failures = 100;
        assert_eq!(scheduler.current_deferral_backoff(), Duration::from_secs(5));
    }

    #[test]
    fn test_record_spawn_resets_consecutive_failures() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = DagScheduler::new(
            graph,
            &make_config(),
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        scheduler.consecutive_spawn_failures = 3;
        let task_id = TaskId(0);
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.record_spawn(task_id, "handle-1".into(), "worker".into());

        assert_eq!(scheduler.consecutive_spawn_failures, 0);
    }

    #[test]
    fn test_record_spawn_failure_reverts_to_ready_no_counter_change() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = DagScheduler::new(
            graph,
            &make_config(),
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        assert_eq!(scheduler.consecutive_spawn_failures, 0);
        let task_id = TaskId(0);
        scheduler.graph.tasks[0].status = TaskStatus::Running;

        let error = SubAgentError::ConcurrencyLimit { active: 1, max: 1 };
        scheduler.record_spawn_failure(task_id, &error);

        assert_eq!(scheduler.consecutive_spawn_failures, 0);
        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Ready);
    }

    #[test]
    fn test_parallel_dispatch_all_ready() {
        let nodes: Vec<_> = (0..6).map(|i| make_node(i, &[])).collect();
        let graph = graph_from_nodes(nodes);
        let config = zeph_config::OrchestrationConfig {
            max_parallel: 2,
            ..make_config()
        };
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        let actions = scheduler.tick();
        let spawn_count = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
            .count();
        assert_eq!(
            spawn_count, 2,
            "only max_parallel=2 tasks dispatched per tick"
        );

        let running_count = scheduler
            .graph
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Running)
            .count();
        assert_eq!(running_count, 2, "only 2 tasks marked Running");
    }

    #[test]
    fn test_batch_backoff_partial_success() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);
        scheduler.consecutive_spawn_failures = 3;

        scheduler.record_batch_backoff(true, true);
        assert_eq!(
            scheduler.consecutive_spawn_failures, 0,
            "any success in batch must reset counter"
        );
    }

    #[test]
    fn test_batch_backoff_all_failed() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);
        scheduler.consecutive_spawn_failures = 2;

        scheduler.record_batch_backoff(false, true);
        assert_eq!(
            scheduler.consecutive_spawn_failures, 3,
            "all-failure tick must increment counter"
        );
    }

    #[test]
    fn test_batch_backoff_no_spawns() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);
        scheduler.consecutive_spawn_failures = 5;

        scheduler.record_batch_backoff(false, false);
        assert_eq!(
            scheduler.consecutive_spawn_failures, 5,
            "no spawns must not change counter"
        );
    }

    #[test]
    fn test_buffer_guard_uses_task_count() {
        let nodes: Vec<_> = (0..10).map(|i| make_node(i, &[])).collect();
        let graph = graph_from_nodes(nodes);
        let config = zeph_config::OrchestrationConfig {
            max_parallel: 2,
            ..make_config()
        };
        let scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();
        assert_eq!(scheduler.graph.tasks.len() * 2, 20);
        assert_eq!(scheduler.max_parallel * 2, 4);
    }

    #[test]
    fn test_batch_mixed_concurrency_and_fatal_failure() {
        use crate::graph::FailureStrategy;

        let mut nodes = vec![make_node(0, &[]), make_node(1, &[])];
        nodes[1].failure_strategy = Some(FailureStrategy::Skip);
        let graph = graph_from_nodes(nodes);
        let mut scheduler = make_scheduler(graph);

        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.graph.tasks[1].status = TaskStatus::Running;

        let concurrency_err = SubAgentError::ConcurrencyLimit { active: 1, max: 1 };
        let actions0 = scheduler.record_spawn_failure(TaskId(0), &concurrency_err);
        assert!(
            actions0.is_empty(),
            "ConcurrencyLimit must produce no extra actions"
        );
        assert_eq!(
            scheduler.graph.tasks[0].status,
            TaskStatus::Ready,
            "task 0 must revert to Ready"
        );

        let fatal_err = SubAgentError::Spawn("provider unavailable".to_string());
        let actions1 = scheduler.record_spawn_failure(TaskId(1), &fatal_err);
        assert_eq!(
            scheduler.graph.tasks[1].status,
            TaskStatus::Skipped,
            "task 1: Skip strategy turns Failed into Skipped via propagate_failure"
        );
        assert!(
            actions1
                .iter()
                .all(|a| !matches!(a, SchedulerAction::Done { .. })),
            "no Done action expected: task 0 is still Ready"
        );

        scheduler.consecutive_spawn_failures = 0;
        scheduler.record_batch_backoff(false, true);
        assert_eq!(
            scheduler.consecutive_spawn_failures, 1,
            "batch with only ConcurrencyLimit must increment counter"
        );
    }

    #[test]
    fn test_deadlock_marks_non_terminal_tasks_canceled() {
        let mut nodes = vec![make_node(0, &[]), make_node(1, &[0]), make_node(2, &[0])];
        nodes[0].status = TaskStatus::Failed;
        nodes[1].status = TaskStatus::Pending;
        nodes[2].status = TaskStatus::Pending;

        let mut graph = graph_from_nodes(nodes);
        graph.status = GraphStatus::Failed;

        let mut scheduler = DagScheduler::resume_from(
            graph,
            &make_config(),
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        let actions = scheduler.tick();

        assert!(
            actions.iter().any(|a| matches!(
                a,
                SchedulerAction::Done {
                    status: GraphStatus::Failed
                }
            )),
            "deadlock must emit Done(Failed); got: {actions:?}"
        );
        assert_eq!(scheduler.graph.status, GraphStatus::Failed);
        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Failed);
        assert_eq!(
            scheduler.graph.tasks[1].status,
            TaskStatus::Canceled,
            "Pending task must be Canceled on deadlock"
        );
        assert_eq!(
            scheduler.graph.tasks[2].status,
            TaskStatus::Canceled,
            "Pending task must be Canceled on deadlock"
        );
    }

    #[test]
    fn test_deadlock_not_triggered_when_task_running() {
        let mut nodes = vec![make_node(0, &[]), make_node(1, &[0])];
        nodes[0].status = TaskStatus::Running;
        nodes[0].assigned_agent = Some("handle-1".into());
        nodes[1].status = TaskStatus::Pending;

        let mut graph = graph_from_nodes(nodes);
        graph.status = GraphStatus::Failed;

        let mut scheduler = DagScheduler::resume_from(
            graph,
            &make_config(),
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        let actions = scheduler.tick();

        assert!(
            actions
                .iter()
                .all(|a| !matches!(a, SchedulerAction::Done { .. })),
            "no Done action expected when a task is running; got: {actions:?}"
        );
        assert_eq!(scheduler.graph.status, GraphStatus::Running);
    }
}
