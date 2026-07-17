// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! DAG algorithm primitives: validation, topological sort, ready-task detection,
//! failure propagation, and retry reset.
//!
//! All functions in this module are pure (no I/O) and operate on slices of
//! [`TaskNode`] or mutable references to a [`TaskGraph`].  The
//! [`DagScheduler`] delegates DAG bookkeeping to these helpers.
//!
//! [`DagScheduler`]: crate::scheduler::DagScheduler

use std::collections::VecDeque;

use zeph_common::fidelity::PlannedToolHint;

use super::error::OrchestrationError;
use super::graph::PredicateOutcome;
use super::graph::{
    FailureStrategy, GraphStatus, TaskGraph, TaskId, TaskNode, TaskResult, TaskStatus,
};

/// Validate that the task slice forms a well-structured DAG.
///
/// Checks:
/// - `tasks.len() <= max_tasks` (rejects oversized graphs).
/// - At least one task exists.
/// - `tasks[i].id == TaskId(i)` invariant holds.
/// - No self-references in `depends_on`.
/// - All `depends_on` entries reference valid indices.
/// - No cycles (via topological sort).
/// - At least one root (task with no dependencies).
/// - No task sets both `recovery` and `verify_predicate` (a predicate-gated task must
///   not be recovery-eligible — recovery bypasses the completion-event handler where
///   predicate verification runs).
/// - No task sets both `recovery.state_injection` and `recovery.route_to` (Mode 1 and
///   Mode 2 are mutually exclusive recovery modes on the same node).
/// - Every `recovery.route_to` target references a valid index and is not a
///   self-reroute.
/// - A `recovery.route_to` target has an empty `depends_on` — it may only ever become
///   `Ready` via Mode-2 activation (the crate-internal `try_reroute`), never via the
///   `Pending` arm of [`ready_tasks`]. This is what makes `TaskStatus::Dormant` sound.
/// - A `recovery.route_to` target does not itself set `recovery.route_to` — chained
///   reroutes are unsupported in v1 (fail closed rather than silently mishandle the
///   transitive re-arm semantics on retry).
/// - Every `recovery.route_to` target has exactly one source (rejects `count > 1`,
///   keeping the crate-internal `resolve_dormant_after_terminal`'s source lookup and
///   the retry re-arm pass single-source; N:1 shared fallback fan-in is deferred).
///
/// Also rejects (upgraded from Mode-1's warn) a task that sets `recovery.route_to` when
/// its effective failure strategy (`task.failure_strategy.unwrap_or(default_failure_strategy)`)
/// is `Skip` or `Ask` — those arms never consult recovery, and `route_to`'s on-failure
/// edge must never coincide with the Skip-BFS arm. Still only warns for
/// `recovery.state_injection` under `Skip`/`Ask` (Mode-1, unchanged behavior).
///
/// # Errors
///
/// Returns `OrchestrationError::InvalidGraph` for structural violations,
/// or `OrchestrationError::CycleDetected` if a cycle is found.
#[must_use = "validation result must be checked"]
pub fn validate(
    tasks: &[TaskNode],
    max_tasks: usize,
    default_failure_strategy: FailureStrategy,
) -> Result<(), OrchestrationError> {
    if tasks.len() > max_tasks {
        return Err(OrchestrationError::InvalidGraph(format!(
            "graph has {} tasks, exceeding the limit of {max_tasks}",
            tasks.len()
        )));
    }

    if tasks.is_empty() {
        return Err(OrchestrationError::InvalidGraph(
            "graph has no tasks".to_string(),
        ));
    }

    // route_to target -> count of sources pointing at it (invariant: exactly one).
    let mut route_to_target_counts: std::collections::HashMap<TaskId, usize> =
        std::collections::HashMap::new();

    for (i, task) in tasks.iter().enumerate() {
        // Invariant: tasks[i].id == TaskId(i)
        let expected = u32::try_from(i).map_err(|_| {
            OrchestrationError::InvalidGraph(format!("task index {i} overflows u32"))
        })?;
        if task.id != TaskId(expected) {
            return Err(OrchestrationError::InvalidGraph(format!(
                "task at index {i} has id {task_id} (expected {i})",
                task_id = task.id
            )));
        }

        for dep in &task.depends_on {
            // No self-references
            if *dep == task.id {
                return Err(OrchestrationError::InvalidGraph(format!(
                    "task {i} has a self-reference"
                )));
            }
            // Valid references only
            if dep.index() >= tasks.len() {
                return Err(OrchestrationError::InvalidGraph(format!(
                    "task {i} references non-existent task {dep}"
                )));
            }
        }

        if task.recovery.is_some() && task.verify_predicate.is_some() {
            return Err(OrchestrationError::InvalidGraph(format!(
                "task {i} sets both recovery and verify_predicate — a predicate-gated \
                 task must not be recovery-eligible"
            )));
        }

        if let Some(recovery) = &task.recovery {
            if recovery.state_injection.is_some() && recovery.route_to.is_some() {
                return Err(OrchestrationError::InvalidGraph(format!(
                    "task {i} sets both recovery.state_injection and recovery.route_to — \
                     Mode 1 and Mode 2 recovery are mutually exclusive"
                )));
            }

            validate_route_to(
                i,
                task,
                recovery,
                tasks,
                default_failure_strategy,
                &mut route_to_target_counts,
            )?;

            let effective_strategy = task.failure_strategy.unwrap_or(default_failure_strategy);
            if recovery.state_injection.is_some()
                && matches!(
                    effective_strategy,
                    FailureStrategy::Skip | FailureStrategy::Ask
                )
            {
                tracing::warn!(
                    task_index = i,
                    strategy = ?effective_strategy,
                    "recovery configured but effective failure strategy is Skip/Ask — \
                     recovery is inert"
                );
            }
        }
    }

    for (target, count) in route_to_target_counts {
        if count > 1 {
            return Err(OrchestrationError::InvalidGraph(format!(
                "task {target} is the recovery.route_to target of {count} sources — \
                 exactly one source per target is required (N:1 shared fallback is deferred)"
            )));
        }
    }

    // Cycle detection + root check via toposort. route_to edges are excluded from
    // toposort by construction (they are read from `depends_on`, which route_to never
    // touches) — they are on-failure edges, not dependency edges, so no cycle-detection
    // change is needed here.
    let sorted = toposort(tasks)?;

    // After a successful toposort every task was visited; verify at least one root
    let has_root = tasks.iter().any(|t| t.depends_on.is_empty());
    if !has_root {
        // toposort would have returned CycleDetected already, but be defensive
        return Err(OrchestrationError::CycleDetected);
    }

    let _ = sorted;
    Ok(())
}

/// Validate a single task's Mode-2 `recovery.route_to` configuration. Extracted from
/// [`validate`] (which would otherwise exceed clippy's line-count threshold): checks the
/// target index is in range and not a self-reroute, that the target has an empty
/// `depends_on` (invariant 4 — see [`TaskStatus::Dormant`]), that the target does not
/// itself set `route_to` (no chained reroutes in v1), tallies the target into
/// `route_to_target_counts` for the caller's N:1 post-loop check, and rejects an
/// effective `Skip`/`Ask` failure strategy on the source. No-op when `recovery.route_to`
/// is `None`.
fn validate_route_to(
    i: usize,
    task: &TaskNode,
    recovery: &crate::graph::RecoveryAction,
    tasks: &[TaskNode],
    default_failure_strategy: FailureStrategy,
    route_to_target_counts: &mut std::collections::HashMap<TaskId, usize>,
) -> Result<(), OrchestrationError> {
    let Some(target) = recovery.route_to else {
        return Ok(());
    };

    if target == task.id {
        return Err(OrchestrationError::InvalidGraph(format!(
            "task {i} sets recovery.route_to to itself — self-reroute is not allowed"
        )));
    }
    if target.index() >= tasks.len() {
        return Err(OrchestrationError::InvalidGraph(format!(
            "task {i} sets recovery.route_to to non-existent task {target}"
        )));
    }

    let target_task = &tasks[target.index()];
    if !target_task.depends_on.is_empty() {
        return Err(OrchestrationError::InvalidGraph(format!(
            "task {i} routes to task {target}, but {target} has a non-empty depends_on \
             — a route_to target must only become ready via on-failure activation"
        )));
    }
    if target_task
        .recovery
        .as_ref()
        .is_some_and(|r| r.route_to.is_some())
    {
        return Err(OrchestrationError::InvalidGraph(format!(
            "task {i} routes to task {target}, but {target} itself sets recovery.route_to \
             — chained `route_to` is not supported in v1"
        )));
    }
    *route_to_target_counts.entry(target).or_insert(0) += 1;

    let effective_strategy = task.failure_strategy.unwrap_or(default_failure_strategy);
    if matches!(
        effective_strategy,
        FailureStrategy::Skip | FailureStrategy::Ask
    ) {
        return Err(OrchestrationError::InvalidGraph(format!(
            "task {i} sets recovery.route_to but its effective failure strategy is \
             {effective_strategy:?} — route_to must never coincide with the Skip-BFS \
             or Ask-pause arms"
        )));
    }

    Ok(())
}

/// Topological sort using Kahn's algorithm.
///
/// Returns tasks in dependency order (roots first).
///
/// # Errors
///
/// Returns `OrchestrationError::CycleDetected` if the graph contains a cycle.
pub fn toposort(tasks: &[TaskNode]) -> Result<Vec<TaskId>, OrchestrationError> {
    let n = tasks.len();

    // in_degree[i] = number of dependencies task i has (number of predecessors)
    let mut in_degree = vec![0u32; n];
    for task in tasks {
        in_degree[task.id.index()] = u32::try_from(task.depends_on.len()).map_err(|_| {
            OrchestrationError::InvalidGraph("dependency count overflows u32".to_string())
        })?;
    }

    let mut queue: VecDeque<TaskId> = in_degree
        .iter()
        .enumerate()
        .filter(|(_, d)| **d == 0)
        .map(|(i, _)| u32::try_from(i).map(TaskId))
        .collect::<Result<_, _>>()
        .map_err(|_| OrchestrationError::InvalidGraph("task index overflows u32".to_string()))?;

    // Build reverse adjacency: for each task, which tasks depend on it
    let mut dependents: Vec<Vec<TaskId>> = vec![Vec::new(); n];
    for task in tasks {
        for dep in &task.depends_on {
            dependents[dep.index()].push(task.id);
        }
    }

    let mut order = Vec::with_capacity(n);
    while let Some(id) = queue.pop_front() {
        order.push(id);
        for &dep_id in &dependents[id.index()] {
            in_degree[dep_id.index()] -= 1;
            if in_degree[dep_id.index()] == 0 {
                queue.push_back(dep_id);
            }
        }
    }

    if order.len() != n {
        return Err(OrchestrationError::CycleDetected);
    }

    Ok(order)
}

/// Returns `true` when all predecessor predicates are satisfied for `task`.
///
/// A predecessor blocks the task when it has a `verify_predicate` set **and**
/// its `predicate_outcome` is either absent or failed. Only `Completed`
/// predecessors with `predicate_outcome.passed == true` are considered cleared.
///
/// This is the single authoritative predicate gate — `tick()` calls `ready_tasks()`
/// which calls this helper, so restart-safety is guaranteed by the persisted
/// `predicate_outcome` field on `TaskNode`.
fn all_parents_predicate_clear(task: &TaskNode, graph: &TaskGraph) -> bool {
    task.depends_on.iter().all(|parent_id| {
        let parent = &graph.tasks[parent_id.index()];
        matches!(
            (&parent.verify_predicate, &parent.predicate_outcome),
            // No gate on this parent — pass through.
            (None, _)
            // Gate present and outcome explicitly passed.
            | (Some(_), Some(PredicateOutcome { passed: true, .. }))
        )
    })
}

/// Find tasks that are ready to be scheduled.
///
/// Returns tasks that are either:
/// - In `Ready` status (already marked ready but not yet running), or
/// - In `Pending` status with all dependencies in `Completed` state.
///
/// Additionally, tasks whose predecessors have an uncleared `verify_predicate`
/// gate are excluded regardless of their own status (predicate gate S2 — gate in
/// `ready_tasks()` as single source of truth).
///
/// This makes the function idempotent across scheduler ticks.
#[must_use]
pub fn ready_tasks(graph: &TaskGraph) -> Vec<TaskId> {
    graph
        .tasks
        .iter()
        .filter_map(|task| {
            match task.status {
                // NOTE: this arm intentionally checks predicate clearance only — it does
                // NOT re-check `depends_on` completion (that already happened when the
                // task was transitioned into `Ready`). This bypass is load-bearing for
                // Mode-1 recovery (spec-075 FR-020): a recovered node's dependents sit in
                // `Pending`, not `Ready`, and unblock through the `Pending` arm below (which
                // does check `depends_on` completion) once the recovered node's status
                // flips to `Completed`. A future refactor that adds a `depends_on`
                // re-check here would be redundant for the normal path but must not change
                // dispatch semantics for predicate-gated tasks interacting with recovery.
                TaskStatus::Ready => {
                    if all_parents_predicate_clear(task, graph) {
                        Some(task.id)
                    } else {
                        None
                    }
                }
                TaskStatus::Pending => {
                    // All deps must be Completed to unblock; also predicate gate must be clear.
                    // This is the arm a Mode-1-recovered node's dependents pass through: the
                    // recovered node's status flips to `Completed` synchronously inside
                    // `propagate_failure()`, so `all_deps_done` sees it on the very next tick.
                    let all_deps_done = task
                        .depends_on
                        .iter()
                        .all(|dep_id| graph.tasks[dep_id.index()].status == TaskStatus::Completed);
                    if all_deps_done && all_parents_predicate_clear(task, graph) {
                        Some(task.id)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        })
        .collect()
}

/// Attempt Mode-1 recovery for a failed task.
///
/// If `graph.tasks[failed_id].recovery.state_injection` is set, marks the node
/// [`TaskStatus::Completed`] with the injected value as its [`TaskResult`] and returns
/// `true` — the failure is absorbed, `graph.status` is left untouched (independent
/// branches continue), and the node's dependents unblock on the next
/// [`ready_tasks`] evaluation via the `Pending` arm's `depends_on` completion check.
/// Returns `false` (no mutation) when no recovery is configured.
///
/// Mutates synchronously with no `.await` — this is what makes the same-tick snapshot
/// atomicity durability guarantee hold (spec-075 FR-016).
fn try_recover(graph: &mut TaskGraph, failed_id: TaskId) -> bool {
    let Some(injection) = graph.tasks[failed_id.index()]
        .recovery
        .as_ref()
        .and_then(|r| r.state_injection.clone())
    else {
        return false;
    };
    let node = &mut graph.tasks[failed_id.index()];
    node.status = TaskStatus::Completed;
    node.result = Some(TaskResult {
        output: injection,
        artifacts: Vec::new(),
        duration_ms: 0,
        agent_id: None,
        agent_def: Some("__recovery__".to_string()),
    });
    tracing::info!(
        task_id = %failed_id,
        "orchestration.dag.recover_task: Mode-1 recovery applied"
    );
    true
}

/// Mark every Mode-2 `route_to` fallback target [`TaskStatus::Dormant`], parking it
/// until its source's terminal failure activates it via [`try_reroute`].
///
/// Iterates all tasks; for each with `recovery.route_to == Some(target)`, sets
/// `graph.tasks[target].status = Dormant` **only if `target.status == Pending`**. The
/// `== Pending` guard makes this idempotent and restart-safe: a reloaded graph whose
/// target was already activated (`Ready`/`Running`/`Completed`/`Dormant`) or resolved
/// (`Skipped`) is never re-dormanted.
///
/// Call site: top of `DagScheduler::init_common`, ordered **before** the root-activation
/// loop (a `route_to` target's `depends_on` is empty by `validate` invariant, so the
/// root-activation loop would otherwise flip it straight to `Ready` on a fresh graph).
pub(crate) fn mark_dormant_route_to_targets(graph: &mut TaskGraph) {
    let targets: Vec<TaskId> = graph
        .tasks
        .iter()
        .filter_map(|t| t.recovery.as_ref().and_then(|r| r.route_to))
        .collect();
    for target in targets {
        let node = &mut graph.tasks[target.index()];
        if node.status == TaskStatus::Pending {
            node.status = TaskStatus::Dormant;
        }
    }
}

/// Attempt Mode-2 reroute for a failed task.
///
/// If `graph.tasks[failed_id].recovery.route_to == Some(target)` **and
/// `target.status == Dormant`**: activates the target (`Dormant → Ready`), sets
/// `target.routed_from = Some(failed_id)`, leaves the source `failed_id` terminal
/// `Failed`, and returns `true` — the failure is absorbed exactly like Mode-1
/// (`graph.status` untouched, independent branches continue). Returns `false` (no
/// mutation) when no `route_to` is configured, or when the target is not currently
/// `Dormant` (already activated via another path, or mid-retry re-arm race) — this
/// runtime status guard is load-bearing (spec-075 FR-D-01): it is what stops
/// `try_reroute` from clobbering a live node.
///
/// Mutates synchronously with no `.await` — preserves the same-tick snapshot atomicity
/// durability guarantee (FR-016), same as [`try_recover`].
fn try_reroute(graph: &mut TaskGraph, failed_id: TaskId) -> bool {
    let Some(target) = graph.tasks[failed_id.index()]
        .recovery
        .as_ref()
        .and_then(|r| r.route_to)
    else {
        return false;
    };
    if graph.tasks[target.index()].status != TaskStatus::Dormant {
        tracing::warn!(
            task_id = %failed_id,
            target = %target,
            target_status = %graph.tasks[target.index()].status,
            "orchestration.dag.try_reroute: route_to target is not Dormant, skipping activation"
        );
        return false;
    }
    let node = &mut graph.tasks[target.index()];
    node.status = TaskStatus::Ready;
    node.routed_from = Some(failed_id);
    tracing::info!(
        task_id = %failed_id,
        target = %target,
        "orchestration.dag.try_reroute: Mode-2 reroute activated"
    );
    true
}

/// Mark `seed` and all its transitive non-terminal dependents [`TaskStatus::Skipped`].
///
/// Shared BFS core for the `Skip` failure-strategy arm and
/// [`resolve_dormant_after_terminal`]'s un-triggered-fallback resolution. Returns the
/// `Running` dependents found along the way — the caller must cancel them, because
/// marking a task `Skipped` in the data structure does not stop execution.
///
/// `rev_adj[i]` must contain the IDs of all tasks that depend on task `i`.
fn skip_subtree(graph: &mut TaskGraph, seed: TaskId, rev_adj: &[Vec<TaskId>]) -> Vec<TaskId> {
    let mut to_cancel = Vec::new();
    let mut queue: VecDeque<TaskId> = VecDeque::new();
    queue.push_back(seed);

    while let Some(current) = queue.pop_front() {
        let dependents = rev_adj.get(current.index()).map_or(&[] as &[TaskId], |v| v);
        for &dep_id in dependents {
            if !graph.tasks[dep_id.index()].status.is_terminal() {
                if graph.tasks[dep_id.index()].status == TaskStatus::Running {
                    to_cancel.push(dep_id);
                }
                graph.tasks[dep_id.index()].status = TaskStatus::Skipped;
                queue.push_back(dep_id);
            }
        }
    }

    to_cancel
}

/// Resolve every still-[`TaskStatus::Dormant`] `route_to` fallback whose source has
/// terminalized without rerouting.
///
/// For each `Dormant` task `F`, finds its unique source `S` (`validate` guarantees
/// exactly one). If `S.status.is_terminal()` — it succeeded, or was itself terminalized
/// by an unrelated cascade/skip without ever calling [`try_reroute`] (which would have
/// flipped `F` to `Ready`) — marks `F` [`TaskStatus::Skipped`] and skips its transitive
/// subtree via [`skip_subtree`] (any task depending on `F` would otherwise strand in
/// `Pending` forever, since `F` never reaches `Completed`).
///
/// Call site: top of `check_graph_completion`, **before** the `all_terminal` /
/// deadlock-detection logic — a still-`Dormant` node is non-terminal and excluded from
/// `ready_tasks()`, so without this sweep a successful plan with an untriggered fallback
/// would be misreported as a scheduler deadlock. Returns the list of resolved (skipped)
/// task IDs, purely for caller-side logging; an empty return means no mutation occurred.
pub(crate) fn resolve_dormant_after_terminal(
    graph: &mut TaskGraph,
    rev_adj: &[Vec<TaskId>],
) -> Vec<TaskId> {
    let dormant_sources: Vec<(TaskId, TaskId)> = graph
        .tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Dormant)
        .filter_map(|target| {
            graph
                .tasks
                .iter()
                .find(|t| t.recovery.as_ref().and_then(|r| r.route_to) == Some(target.id))
                .map(|source| (target.id, source.id))
        })
        .collect();

    let mut resolved = Vec::new();
    for (target, source) in dormant_sources {
        if graph.tasks[source.index()].status.is_terminal() {
            graph.tasks[target.index()].status = TaskStatus::Skipped;
            resolved.push(target);
            skip_subtree(graph, target, rev_adj);
        }
    }
    resolved
}

/// Handle a task failure. Applies the effective failure strategy and mutates the graph.
///
/// Returns the list of `Running` task IDs that the caller should cancel (for `Abort` strategy).
///
/// - `Abort`: tries Mode-1 recovery, then Mode-2 reroute; if neither applies, sets
///   `graph.status = Failed` and returns all currently `Running` task IDs.
/// - `Skip`: marks the failed task `Skipped` and transitively skips all non-terminal dependents
///   using BFS over a reverse adjacency list.
/// - `Retry`: if `retry_count < max_retries`, increments counter and resets task to `Ready`.
///   Otherwise tries Mode-1 recovery, then Mode-2 reroute, then falls through to `Abort`.
/// - `Ask`: sets `graph.status = Paused`.
///
/// `rev_adj[i]` must contain the IDs of all tasks that depend on task `i` (pre-built by the
/// caller from `TopologyAnalysis::rev_adj` to avoid repeated allocation on the hot path).
pub fn propagate_failure(
    graph: &mut TaskGraph,
    failed_id: TaskId,
    rev_adj: &[Vec<TaskId>],
) -> Vec<TaskId> {
    // If the task is already terminal (not Failed), this is a no-op
    if graph.tasks[failed_id.index()].status != TaskStatus::Failed {
        return Vec::new();
    }

    // Determine effective strategy
    let strategy = graph.tasks[failed_id.index()]
        .failure_strategy
        .unwrap_or(graph.default_failure_strategy);

    let max_retries = graph.tasks[failed_id.index()]
        .max_retries
        .unwrap_or(graph.default_max_retries);

    match strategy {
        FailureStrategy::Abort => {
            if try_recover(graph, failed_id) {
                return Vec::new();
            }
            if try_reroute(graph, failed_id) {
                return Vec::new();
            }
            graph.status = GraphStatus::Failed;
            // Return IDs of all currently Running tasks for the caller to cancel
            graph
                .tasks
                .iter()
                .filter(|t| t.status == TaskStatus::Running)
                .map(|t| t.id)
                .collect()
        }

        FailureStrategy::Skip => {
            // Mark the failed task as Skipped, then transitively skip all non-terminal
            // dependents. route_to targets are never reached here: they are not
            // `depends_on`-dependents of the failed task (validate invariant), and
            // route_to is rejected under an effective Skip strategy at graph
            // construction time.
            graph.tasks[failed_id.index()].status = TaskStatus::Skipped;
            skip_subtree(graph, failed_id, rev_adj)
        }

        FailureStrategy::Retry => {
            let retry_count = graph.tasks[failed_id.index()].retry_count;
            if retry_count < max_retries {
                graph.tasks[failed_id.index()].retry_count += 1;
                graph.tasks[failed_id.index()].status = TaskStatus::Ready;
                Vec::new()
            } else {
                // Retry exhausted — try Mode-1 recovery, then Mode-2 reroute, before
                // falling through to Abort.
                if try_recover(graph, failed_id) {
                    return Vec::new();
                }
                if try_reroute(graph, failed_id) {
                    return Vec::new();
                }
                graph.status = GraphStatus::Failed;
                graph
                    .tasks
                    .iter()
                    .filter(|t| t.status == TaskStatus::Running)
                    .map(|t| t.id)
                    .collect()
            }
        }

        FailureStrategy::Ask => {
            graph.status = GraphStatus::Paused;
            Vec::new()
        }

        // `FailureStrategy` is `#[non_exhaustive]` (zeph-config), so a wildcard arm is
        // mandatory for compilation even though all current variants are handled above.
        // Unlike the old dead wildcard, this one is loud: it logs the unhandled variant
        // instead of silently falling back to Abort-equivalent behavior.
        strategy => {
            tracing::error!(
                ?strategy,
                "unhandled failure strategy variant, defaulting to Abort"
            );
            graph.status = GraphStatus::Failed;
            graph
                .tasks
                .iter()
                .filter(|t| t.status == TaskStatus::Running)
                .map(|t| t.id)
                .collect()
        }
    }
}

/// Reset a graph for retry after it has entered `Failed` or `Paused` status.
///
/// - Resets all `Failed` tasks to `Ready` (and clears `retry_count`).
/// - Resets all `Canceled` tasks to `Pending` (IC2: after an Abort cascade,
///   running tasks are marked `Canceled`; without this they block their dependents).
/// - BFS resets all `Skipped` tasks downstream of a failed/canceled task back to
///   `Pending`, allowing `ready_tasks()` to re-evaluate them on the next tick.
/// - Sets `graph.status = Running` so the scheduler can continue.
///
/// `rev_adj[i]` must contain the IDs of all tasks that depend on task `i` (pre-built by the
/// caller from `TopologyAnalysis::rev_adj` to avoid repeated allocation on the hot path).
///
/// # Errors
///
/// Returns `OrchestrationError::InvalidGraph` if the graph is not in `Failed`
/// or `Paused` status (the only states that make sense to retry from).
pub fn reset_for_retry(
    graph: &mut TaskGraph,
    rev_adj: &[Vec<TaskId>],
) -> Result<(), OrchestrationError> {
    use super::graph::GraphStatus;

    if graph.status != GraphStatus::Failed && graph.status != GraphStatus::Paused {
        return Err(OrchestrationError::InvalidGraph(format!(
            "cannot retry graph in status {}; only Failed or Paused graphs can be retried",
            graph.status
        )));
    }

    // First pass: reset Failed -> Ready and collect their IDs as BFS seeds.
    let mut seeds: Vec<TaskId> = Vec::new();
    for task in &mut graph.tasks {
        if task.status == TaskStatus::Failed {
            task.status = TaskStatus::Ready;
            task.retry_count = 0;
            seeds.push(task.id);
        }
    }

    // IC2: reset Canceled tasks (produced by Abort cascade) to Pending so their
    // dependents are not permanently blocked.  These are NOT seeds for the BFS
    // (they were not the direct cause of the failure chain) but must be re-runnable.
    for task in &mut graph.tasks {
        if task.status == TaskStatus::Canceled {
            task.status = TaskStatus::Pending;
        }
    }

    if seeds.is_empty() {
        // Paused with no failed tasks (e.g., Ask strategy hit); just resume.
        graph.status = GraphStatus::Running;
        return Ok(());
    }

    // `seeds` is moved into the Skipped-BFS queue below; the route_to re-arm pass
    // (D2, spec-075 FR-D-01) needs its own copy of the just-reset Failed source IDs.
    let seeds_for_reroute = seeds.clone();

    // BFS from seeds: reset Skipped dependents back to Pending.
    let mut queue: std::collections::VecDeque<TaskId> = seeds.into_iter().collect();
    while let Some(current) = queue.pop_front() {
        let dependents = rev_adj.get(current.index()).map_or(&[] as &[TaskId], |v| v);
        for &dep_id in dependents {
            if graph.tasks[dep_id.index()].status == TaskStatus::Skipped {
                graph.tasks[dep_id.index()].status = TaskStatus::Pending;
                queue.push_back(dep_id);
            }
        }
    }

    // route_to re-arm pass (D2, spec-075 FR-D-01): a rerouted source that is reset to
    // Ready must re-arm its entire fallback branch back to the parked/quiescent state,
    // else a source that now succeeds finds its fallback already Ready/beyond and
    // dispatches anyway (defeating Mode 2), or a source that fails again finds its
    // fallback not Dormant and `try_reroute`'s runtime guard refuses to re-activate it
    // (permanently disabling Mode 2 for that source/target pair). This is a SEPARATE
    // pass from the Skipped-BFS above, keyed on `route_to` rather than `depends_on`:
    // a route_to target is never a `depends_on`-dependent of its source (validate
    // invariant forces the target's `depends_on` empty), so the Skipped-BFS provably
    // never reaches it — walking `rev_adj` from the target's own subtree is required.
    for s_id in seeds_for_reroute {
        let Some(target) = graph.tasks[s_id.index()]
            .recovery
            .as_ref()
            .and_then(|r| r.route_to)
        else {
            continue;
        };

        let target_node = &mut graph.tasks[target.index()];
        target_node.status = TaskStatus::Dormant;
        target_node.routed_from = None;
        target_node.retry_count = 0;
        target_node.result = None;

        // BFS the target's own transitive dependents, resetting any non-Pending status
        // (Completed/Failed/Skipped/Canceled/Ready/Running left by a prior fallback
        // run) back to Pending for a clean re-run. Idempotent: a target that never ran
        // has dependents already Pending/Dormant, so this is a no-op.
        let mut re_arm_queue: VecDeque<TaskId> = VecDeque::new();
        re_arm_queue.push_back(target);
        while let Some(current) = re_arm_queue.pop_front() {
            let dependents = rev_adj.get(current.index()).map_or(&[] as &[TaskId], |v| v);
            for &dep_id in dependents {
                let dep = &mut graph.tasks[dep_id.index()];
                if dep.status != TaskStatus::Pending {
                    dep.status = TaskStatus::Pending;
                    dep.retry_count = 0;
                    dep.result = None;
                    re_arm_queue.push_back(dep_id);
                }
            }
        }
    }

    graph.status = GraphStatus::Running;
    Ok(())
}

/// Stopwords filtered out of task keyword extraction.
const KEYWORD_STOPWORDS: &[&str] = &["the", "a", "an", "in", "of", "for", "to", "from", "with"];

/// Extract lookahead tool hints from the DAG for PAACE context scoring.
///
/// Performs a BFS forward from all tasks currently in `Running` or `Ready`
/// status (the execution frontier, distance 0) and collects downstream tasks
/// at distances 1..=`depth` as [`PlannedToolHint`] values.
///
/// # Arguments
///
/// * `graph` — the active task graph.
/// * `depth` — maximum lookahead steps. `0` means "disabled" and returns an
///   empty vec immediately without traversing the graph.
///
/// # Returns
///
/// A [`Vec<PlannedToolHint>`] sorted by `distance_from_current` ascending.
/// Returns an empty vec when `depth == 0`, when no Running/Ready frontier
/// tasks exist, or when there are no reachable downstream tasks within `depth`.
///
/// # Examples
///
/// ```rust
/// use zeph_orchestration::{TaskGraph, TaskNode, TaskStatus};
/// use zeph_orchestration::dag::lookahead_tools;
///
/// let mut g = TaskGraph::new("example");
/// g.tasks.push(TaskNode::new(0, "search", "web search"));
/// g.tasks.push(TaskNode::new(1, "summarize", "summarize results"));
/// g.tasks[1].depends_on = vec![zeph_orchestration::TaskId(0)];
/// g.tasks[0].status = TaskStatus::Running;
/// g.tasks[1].status = TaskStatus::Pending;
///
/// let hints = lookahead_tools(&g, 1);
/// assert_eq!(hints.len(), 1);
/// assert_eq!(hints[0].tool_name, "summarize");
/// assert_eq!(hints[0].distance_from_current, 1);
/// ```
#[must_use]
pub fn lookahead_tools(graph: &TaskGraph, depth: u8) -> Vec<PlannedToolHint> {
    let _span = tracing::debug_span!("orch.dag.lookahead", depth = depth).entered();

    if depth == 0 {
        return vec![];
    }

    let tasks = &graph.tasks;
    let n = tasks.len();

    // Build forward adjacency: rev_adj[i] = tasks that depend on task i (downstream).
    let mut forward_adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for task in tasks {
        for dep in &task.depends_on {
            forward_adj[dep.index()].push(task.id.index());
        }
    }

    // BFS from Running/Ready frontier (distance=0, not emitted).
    let mut visited = vec![false; n];
    let mut queue: VecDeque<(usize, u8)> = VecDeque::new();

    for task in tasks {
        if matches!(task.status, TaskStatus::Running | TaskStatus::Ready) {
            visited[task.id.index()] = true;
            queue.push_back((task.id.index(), 0));
        }
    }

    if queue.is_empty() {
        return vec![];
    }

    let mut hints: Vec<PlannedToolHint> = Vec::new();

    while let Some((idx, dist)) = queue.pop_front() {
        for &child_idx in &forward_adj[idx] {
            if visited[child_idx] {
                continue;
            }
            visited[child_idx] = true;
            let child_dist = dist + 1;
            if child_dist <= depth {
                let child = &tasks[child_idx];
                let tool_name = child.agent_hint.as_deref().unwrap_or(&child.title);
                hints.push(PlannedToolHint::new(
                    tool_name,
                    extract_keywords(tool_name, &child.description),
                    child_dist,
                ));
                queue.push_back((child_idx, child_dist));
            }
        }
    }

    hints.sort_by_key(|h| h.distance_from_current);
    hints
}

/// Extract up to 10 keywords from a tool name and task description prefix.
///
/// The full `tool_name` is always inserted first (enables exact matching by
/// the fidelity scorer). Split tokens from `title` and `description` follow,
/// lowercased, filtered for stopwords and minimum length, deduplicated, capped
/// at 10 total entries.
fn extract_keywords(tool_name: &str, description: &str) -> Vec<String> {
    let end = description.floor_char_boundary(200);
    let desc_prefix = &description[..end];
    let combined = format!("{tool_name} {desc_prefix}");

    let mut seen = std::collections::HashSet::new();
    let mut keywords: Vec<String> = Vec::new();

    // Always include the full tool_name first for exact matching.
    let full = tool_name.to_lowercase();
    seen.insert(full.clone());
    keywords.push(full);

    for token in combined.split(|c: char| !c.is_alphanumeric()) {
        if keywords.len() == 10 {
            break;
        }
        if token.len() < 3 {
            continue;
        }
        let lower = token.to_lowercase();
        if KEYWORD_STOPWORDS.contains(&lower.as_str()) {
            continue;
        }
        if seen.insert(lower.clone()) {
            keywords.push(lower);
        }
    }

    keywords
}

/// Inject new tasks into a task graph, validate DAG acyclicity, and mark new
/// roots as `Ready`.
///
/// Does NOT re-analyze topology — topology re-analysis is deferred to the next
/// `tick()` via the `dirty` flag in `DagScheduler` (critic C2).
///
/// # Errors
///
/// Returns `OrchestrationError::VerificationFailed` if the resulting graph
/// contains a cycle or exceeds the task limit.
pub fn inject_tasks(
    graph: &mut TaskGraph,
    new_tasks: Vec<TaskNode>,
    max_tasks: usize,
) -> Result<(), OrchestrationError> {
    if new_tasks.is_empty() {
        return Ok(());
    }

    let existing_len = graph.tasks.len();
    let total = existing_len + new_tasks.len();

    if total > max_tasks {
        return Err(OrchestrationError::VerificationFailed(format!(
            "inject_tasks would create {total} tasks, exceeding limit of {max_tasks}"
        )));
    }

    for (i, task) in new_tasks.iter().enumerate() {
        let expected = TaskId(u32::try_from(existing_len + i).map_err(|_| {
            OrchestrationError::VerificationFailed("task index overflows u32".to_string())
        })?);
        if task.id != expected {
            return Err(OrchestrationError::VerificationFailed(format!(
                "injected task at position {} has id {} (expected {})",
                i, task.id, expected
            )));
        }
    }

    graph.tasks.extend(new_tasks);

    validate(&graph.tasks, max_tasks, graph.default_failure_strategy).map_err(|e| match e {
        OrchestrationError::CycleDetected => {
            OrchestrationError::VerificationFailed("inject_tasks introduced a cycle".to_string())
        }
        other => OrchestrationError::VerificationFailed(other.to_string()),
    })?;

    let n = graph.tasks.len();
    for i in existing_len..n {
        let all_deps_done = graph.tasks[i]
            .depends_on
            .iter()
            .all(|dep| graph.tasks[dep.index()].status == TaskStatus::Completed);
        if all_deps_done {
            graph.tasks[i].status = TaskStatus::Ready;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{FailureStrategy, GraphStatus, TaskGraph, TaskNode, TaskStatus};
    use crate::topology::build_rev_adj;
    use std::assert_matches;

    fn make_node(id: u32, deps: &[u32]) -> TaskNode {
        let mut n = TaskNode::new(id, format!("task-{id}"), "desc");
        n.depends_on = deps.iter().map(|&d| TaskId(d)).collect();
        n
    }

    fn graph_from_nodes(nodes: Vec<TaskNode>) -> TaskGraph {
        let mut g = TaskGraph::new("test");
        g.tasks = nodes;
        g
    }

    fn make_rev_adj(graph: &TaskGraph) -> Vec<Vec<TaskId>> {
        build_rev_adj(&graph.tasks)
    }

    // --- validate tests ---

    #[test]
    fn test_validate_empty_graph() {
        let err = validate(&[], 20, FailureStrategy::Abort).unwrap_err();
        assert_matches!(err, OrchestrationError::InvalidGraph(_));
    }

    #[test]
    fn test_validate_exceeds_max_tasks() {
        let tasks: Vec<TaskNode> = (0..5).map(|i| make_node(i, &[])).collect();
        let err = validate(&tasks, 3, FailureStrategy::Abort).unwrap_err();
        assert_matches!(err, OrchestrationError::InvalidGraph(_));
    }

    #[test]
    fn test_validate_single_task_no_deps() {
        let tasks = vec![make_node(0, &[])];
        assert!(validate(&tasks, 20, FailureStrategy::Abort).is_ok());
    }

    #[test]
    fn test_validate_self_reference() {
        let mut tasks = vec![make_node(0, &[])];
        tasks[0].depends_on = vec![TaskId(0)];
        let err = validate(&tasks, 20, FailureStrategy::Abort).unwrap_err();
        assert_matches!(err, OrchestrationError::InvalidGraph(_));
    }

    #[test]
    fn test_validate_invalid_taskid_reference() {
        let mut tasks = vec![make_node(0, &[])];
        tasks[0].depends_on = vec![TaskId(99)];
        let err = validate(&tasks, 20, FailureStrategy::Abort).unwrap_err();
        assert_matches!(err, OrchestrationError::InvalidGraph(_));
    }

    #[test]
    fn test_validate_linear_chain() {
        // A(0) -> B(1) -> C(2)
        let tasks = vec![make_node(0, &[]), make_node(1, &[0]), make_node(2, &[1])];
        assert!(validate(&tasks, 20, FailureStrategy::Abort).is_ok());
    }

    #[test]
    fn test_validate_diamond() {
        // A(0) -> B(1), A(0) -> C(2), B(1) -> D(3), C(2) -> D(3)
        let tasks = vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[0]),
            make_node(3, &[1, 2]),
        ];
        assert!(validate(&tasks, 20, FailureStrategy::Abort).is_ok());
    }

    #[test]
    fn test_validate_cycle_two_nodes() {
        // A(0) depends on B(1), B(1) depends on A(0)
        let tasks = vec![make_node(0, &[1]), make_node(1, &[0])];
        let err = validate(&tasks, 20, FailureStrategy::Abort).unwrap_err();
        assert_matches!(err, OrchestrationError::CycleDetected);
    }

    #[test]
    fn test_validate_cycle_three_nodes() {
        // A(0)->B(1)->C(2)->A(0)
        let tasks = vec![make_node(0, &[2]), make_node(1, &[0]), make_node(2, &[1])];
        let err = validate(&tasks, 20, FailureStrategy::Abort).unwrap_err();
        assert_matches!(err, OrchestrationError::CycleDetected);
    }

    #[test]
    fn test_validate_taskid_invariant() {
        let mut tasks = vec![make_node(0, &[]), make_node(1, &[0])];
        // Break invariant: tasks[1] should have id TaskId(1) but we set TaskId(5)
        tasks[1].id = TaskId(5);
        let err = validate(&tasks, 20, FailureStrategy::Abort).unwrap_err();
        assert_matches!(err, OrchestrationError::InvalidGraph(_));
    }

    // --- recovery validation guard tests ---

    #[test]
    fn test_validate_rejects_recovery_with_verify_predicate() {
        let mut tasks = vec![make_node(0, &[])];
        tasks[0].recovery = Some(crate::graph::RecoveryAction {
            state_injection: Some("fallback".to_string()),
            route_to: None,
        });
        tasks[0].verify_predicate = Some(crate::graph::VerifyPredicate::Natural(
            "criterion".to_string(),
        ));
        let err = validate(&tasks, 20, FailureStrategy::Abort).unwrap_err();
        assert_matches!(err, OrchestrationError::InvalidGraph(_));
    }

    #[test]
    fn test_validate_recovery_alone_is_ok() {
        let mut tasks = vec![make_node(0, &[])];
        tasks[0].recovery = Some(crate::graph::RecoveryAction {
            state_injection: Some("fallback".to_string()),
            route_to: None,
        });
        assert!(validate(&tasks, 20, FailureStrategy::Abort).is_ok());
    }

    #[test]
    fn test_validate_recovery_with_skip_strategy_warns_but_ok() {
        let mut tasks = vec![make_node(0, &[])];
        tasks[0].recovery = Some(crate::graph::RecoveryAction {
            state_injection: Some("fallback".to_string()),
            route_to: None,
        });
        tasks[0].failure_strategy = Some(FailureStrategy::Skip);
        assert!(validate(&tasks, 20, FailureStrategy::Abort).is_ok());
    }

    #[test]
    fn test_validate_recovery_with_ask_strategy_warns_but_ok() {
        let mut tasks = vec![make_node(0, &[])];
        tasks[0].recovery = Some(crate::graph::RecoveryAction {
            state_injection: Some("fallback".to_string()),
            route_to: None,
        });
        tasks[0].failure_strategy = Some(FailureStrategy::Ask);
        assert!(validate(&tasks, 20, FailureStrategy::Abort).is_ok());
    }

    #[test]
    fn test_validate_recovery_with_abort_or_retry_no_warning_ok() {
        let mut tasks = vec![make_node(0, &[])];
        tasks[0].recovery = Some(crate::graph::RecoveryAction {
            state_injection: Some("fallback".to_string()),
            route_to: None,
        });
        tasks[0].failure_strategy = Some(FailureStrategy::Retry);
        assert!(validate(&tasks, 20, FailureStrategy::Abort).is_ok());

        // Also verify the graph-default-strategy path (no per-task override) is Ok.
        let mut tasks2 = vec![make_node(0, &[])];
        tasks2[0].recovery = Some(crate::graph::RecoveryAction {
            state_injection: Some("fallback".to_string()),
            route_to: None,
        });
        assert!(validate(&tasks2, 20, FailureStrategy::Abort).is_ok());
    }

    // --- route_to (Mode 2) validate tests (spec-075 FR-D-01) ---

    fn make_route_to_pair() -> Vec<TaskNode> {
        // B(1) routes to F(0). Both have empty depends_on: F per invariant (4), and B
        // because route_to is an on-failure edge, not a dependency edge -- B must NOT
        // depend on F (that would be the rejected N5 topology).
        let mut tasks = vec![make_node(0, &[]), make_node(1, &[])];
        tasks[1].recovery = Some(crate::graph::RecoveryAction {
            state_injection: None,
            route_to: Some(TaskId(0)),
        });
        tasks
    }

    #[test]
    fn test_validate_route_to_valid_pair_ok() {
        let tasks = make_route_to_pair();
        assert!(validate(&tasks, 20, FailureStrategy::Abort).is_ok());
    }

    #[test]
    fn test_validate_route_to_self_reroute_rejected() {
        let mut tasks = vec![make_node(0, &[])];
        tasks[0].recovery = Some(crate::graph::RecoveryAction {
            state_injection: None,
            route_to: Some(TaskId(0)),
        });
        let err = validate(&tasks, 20, FailureStrategy::Abort).unwrap_err();
        assert_matches!(err, OrchestrationError::InvalidGraph(_));
    }

    #[test]
    fn test_validate_route_to_out_of_range_rejected() {
        let mut tasks = vec![make_node(0, &[])];
        tasks[0].recovery = Some(crate::graph::RecoveryAction {
            state_injection: None,
            route_to: Some(TaskId(99)),
        });
        let err = validate(&tasks, 20, FailureStrategy::Abort).unwrap_err();
        assert_matches!(err, OrchestrationError::InvalidGraph(_));
    }

    #[test]
    fn test_validate_route_to_and_state_injection_mutually_exclusive() {
        let mut tasks = make_route_to_pair();
        tasks[1].recovery.as_mut().unwrap().state_injection = Some("fallback".to_string());
        let err = validate(&tasks, 20, FailureStrategy::Abort).unwrap_err();
        assert_matches!(err, OrchestrationError::InvalidGraph(_));
    }

    #[test]
    fn test_validate_route_to_target_with_deps_rejected() {
        // F(0) must have empty depends_on; give it one.
        let mut tasks = vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[1]), // F=2, but depends on 1 -- invalid target
        ];
        tasks[1].recovery = Some(crate::graph::RecoveryAction {
            state_injection: None,
            route_to: Some(TaskId(2)),
        });
        let err = validate(&tasks, 20, FailureStrategy::Abort).unwrap_err();
        assert_matches!(err, OrchestrationError::InvalidGraph(_));
    }

    #[test]
    fn test_validate_route_to_chained_rejected() {
        // M3: F itself must not set route_to (chained reroute unsupported in v1).
        let mut tasks = vec![
            make_node(0, &[]), // F2 (final target)
            make_node(1, &[]), // F (chains to F2)
            make_node(2, &[]), // B (routes to F)
        ];
        tasks[1].recovery = Some(crate::graph::RecoveryAction {
            state_injection: None,
            route_to: Some(TaskId(0)),
        });
        tasks[2].recovery = Some(crate::graph::RecoveryAction {
            state_injection: None,
            route_to: Some(TaskId(1)),
        });
        let err = validate(&tasks, 20, FailureStrategy::Abort).unwrap_err();
        assert_matches!(err, OrchestrationError::InvalidGraph(_));
    }

    #[test]
    fn test_validate_route_to_n_to_one_rejected() {
        // Two sources routing to the same target F.
        let mut tasks = vec![
            make_node(0, &[]), // F
            make_node(1, &[]), // source 1
            make_node(2, &[]), // source 2
        ];
        tasks[1].recovery = Some(crate::graph::RecoveryAction {
            state_injection: None,
            route_to: Some(TaskId(0)),
        });
        tasks[2].recovery = Some(crate::graph::RecoveryAction {
            state_injection: None,
            route_to: Some(TaskId(0)),
        });
        let err = validate(&tasks, 20, FailureStrategy::Abort).unwrap_err();
        assert_matches!(err, OrchestrationError::InvalidGraph(_));
    }

    #[test]
    fn test_validate_route_to_under_skip_strategy_rejected() {
        // Upgraded from Mode-1's warn to a hard error for route_to.
        let mut tasks = make_route_to_pair();
        tasks[1].failure_strategy = Some(FailureStrategy::Skip);
        let err = validate(&tasks, 20, FailureStrategy::Abort).unwrap_err();
        assert_matches!(err, OrchestrationError::InvalidGraph(_));
    }

    #[test]
    fn test_validate_route_to_under_ask_strategy_rejected() {
        let mut tasks = make_route_to_pair();
        tasks[1].failure_strategy = Some(FailureStrategy::Ask);
        let err = validate(&tasks, 20, FailureStrategy::Abort).unwrap_err();
        assert_matches!(err, OrchestrationError::InvalidGraph(_));
    }

    #[test]
    fn test_validate_route_to_under_default_skip_strategy_rejected() {
        // Effective strategy via graph default (no per-task override) must also reject.
        let tasks = make_route_to_pair();
        let err = validate(&tasks, 20, FailureStrategy::Skip).unwrap_err();
        assert_matches!(err, OrchestrationError::InvalidGraph(_));
    }

    // --- mark_dormant_route_to_targets tests ---

    #[test]
    fn test_mark_dormant_route_to_targets_marks_pending_target() {
        let mut graph = graph_from_nodes(make_route_to_pair());
        mark_dormant_route_to_targets(&mut graph);
        assert_eq!(graph.tasks[0].status, TaskStatus::Dormant);
    }

    #[test]
    fn test_mark_dormant_route_to_targets_guard_skips_non_pending() {
        let mut graph = graph_from_nodes(make_route_to_pair());
        graph.tasks[0].status = TaskStatus::Completed;
        mark_dormant_route_to_targets(&mut graph);
        assert_eq!(
            graph.tasks[0].status,
            TaskStatus::Completed,
            "guard must not re-dormant an already-terminal target"
        );
    }

    #[test]
    fn test_mark_dormant_route_to_targets_no_route_to_is_noop() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        mark_dormant_route_to_targets(&mut graph);
        assert_eq!(graph.tasks[0].status, TaskStatus::Pending);
    }

    // --- try_reroute / propagate_failure Mode-2 tests ---

    #[test]
    fn test_propagate_failure_abort_reroutes_to_dormant_target() {
        let mut graph = graph_from_nodes(make_route_to_pair());
        graph.status = GraphStatus::Running;
        graph.tasks[0].status = TaskStatus::Dormant;
        graph.tasks[1].status = TaskStatus::Failed;
        graph.tasks[1].failure_strategy = Some(FailureStrategy::Abort);

        let __ra = make_rev_adj(&graph);
        let to_cancel = propagate_failure(&mut graph, TaskId(1), &__ra);

        assert!(to_cancel.is_empty());
        assert_eq!(
            graph.tasks[1].status,
            TaskStatus::Failed,
            "source stays terminal Failed"
        );
        assert_eq!(graph.tasks[0].status, TaskStatus::Ready, "target activated");
        assert_eq!(graph.tasks[0].routed_from, Some(TaskId(1)));
        assert_eq!(
            graph.status,
            GraphStatus::Running,
            "graph.status must be left untouched by reroute"
        );
    }

    #[test]
    fn test_propagate_failure_retry_exhausted_reroutes_to_dormant_target() {
        let mut graph = graph_from_nodes(make_route_to_pair());
        graph.status = GraphStatus::Running;
        graph.tasks[0].status = TaskStatus::Dormant;
        graph.tasks[1].status = TaskStatus::Failed;
        graph.tasks[1].failure_strategy = Some(FailureStrategy::Retry);
        graph.tasks[1].max_retries = Some(3);
        graph.tasks[1].retry_count = 3;

        let __ra = make_rev_adj(&graph);
        propagate_failure(&mut graph, TaskId(1), &__ra);

        assert_eq!(graph.tasks[0].status, TaskStatus::Ready);
        assert_eq!(graph.tasks[0].routed_from, Some(TaskId(1)));
        assert_eq!(graph.status, GraphStatus::Running);
    }

    #[test]
    fn test_propagate_failure_reroute_runtime_guard_refuses_non_dormant_target() {
        // Target already Ready (e.g. re-arm race / already activated) — try_reroute
        // must refuse to clobber it and fall through to Abort instead.
        let mut graph = graph_from_nodes(make_route_to_pair());
        graph.status = GraphStatus::Running;
        graph.tasks[0].status = TaskStatus::Ready; // NOT Dormant
        graph.tasks[1].status = TaskStatus::Failed;
        graph.tasks[1].failure_strategy = Some(FailureStrategy::Abort);

        let __ra = make_rev_adj(&graph);
        propagate_failure(&mut graph, TaskId(1), &__ra);

        assert_eq!(
            graph.tasks[0].status,
            TaskStatus::Ready,
            "runtime guard must not mutate a non-Dormant target"
        );
        assert_eq!(graph.tasks[0].routed_from, None);
        assert_eq!(
            graph.status,
            GraphStatus::Failed,
            "must fall through to Abort when reroute is refused"
        );
    }

    #[test]
    fn test_route_to_target_dependent_becomes_ready_after_reroute() {
        // F(0) <- routed by B(1); G(2) depends_on F(0).
        let mut graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[0]),
        ]);
        graph.tasks[1].recovery = Some(crate::graph::RecoveryAction {
            state_injection: None,
            route_to: Some(TaskId(0)),
        });
        graph.status = GraphStatus::Running;
        graph.tasks[0].status = TaskStatus::Dormant;
        graph.tasks[1].status = TaskStatus::Failed;
        graph.tasks[1].failure_strategy = Some(FailureStrategy::Abort);

        let __ra = make_rev_adj(&graph);
        propagate_failure(&mut graph, TaskId(1), &__ra);
        assert_eq!(graph.tasks[0].status, TaskStatus::Ready);

        // F completes -> G must unblock via the normal Pending arm.
        graph.tasks[0].status = TaskStatus::Completed;
        let ready = ready_tasks(&graph);
        assert!(ready.contains(&TaskId(2)));
    }

    // --- resolve_dormant_after_terminal tests ---

    #[test]
    fn test_resolve_dormant_after_terminal_skips_untriggered_fallback_on_source_success() {
        let mut graph = graph_from_nodes(make_route_to_pair());
        graph.tasks[0].status = TaskStatus::Dormant;
        graph.tasks[1].status = TaskStatus::Completed; // source succeeded, never rerouted

        let __ra = make_rev_adj(&graph);
        let resolved = resolve_dormant_after_terminal(&mut graph, &__ra);

        assert_eq!(resolved, vec![TaskId(0)]);
        assert_eq!(graph.tasks[0].status, TaskStatus::Skipped);
    }

    #[test]
    fn test_resolve_dormant_after_terminal_skips_subtree() {
        // F(0) <- routed by B(1); G(2) depends_on F(0). Source succeeds without
        // rerouting: F must resolve Skipped and drag G down with it.
        let mut graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[0]),
        ]);
        graph.tasks[1].recovery = Some(crate::graph::RecoveryAction {
            state_injection: None,
            route_to: Some(TaskId(0)),
        });
        graph.tasks[0].status = TaskStatus::Dormant;
        graph.tasks[1].status = TaskStatus::Completed;
        graph.tasks[2].status = TaskStatus::Pending;

        let __ra = make_rev_adj(&graph);
        resolve_dormant_after_terminal(&mut graph, &__ra);

        assert_eq!(graph.tasks[0].status, TaskStatus::Skipped);
        assert_eq!(
            graph.tasks[2].status,
            TaskStatus::Skipped,
            "F's downstream subtree must be skipped when the fallback is never triggered"
        );
    }

    #[test]
    fn test_resolve_dormant_after_terminal_noop_while_source_running() {
        let mut graph = graph_from_nodes(make_route_to_pair());
        graph.tasks[0].status = TaskStatus::Dormant;
        graph.tasks[1].status = TaskStatus::Running; // not terminal yet

        let __ra = make_rev_adj(&graph);
        let resolved = resolve_dormant_after_terminal(&mut graph, &__ra);

        assert!(resolved.is_empty());
        assert_eq!(graph.tasks[0].status, TaskStatus::Dormant);
    }

    #[test]
    fn test_resolve_dormant_after_terminal_ignores_activated_target() {
        // Target already Ready (activated by a prior reroute) must never be touched
        // by the sweep, even if its source is terminal-Failed.
        let mut graph = graph_from_nodes(make_route_to_pair());
        graph.tasks[0].status = TaskStatus::Ready;
        graph.tasks[0].routed_from = Some(TaskId(1));
        graph.tasks[1].status = TaskStatus::Failed;

        let __ra = make_rev_adj(&graph);
        let resolved = resolve_dormant_after_terminal(&mut graph, &__ra);

        assert!(resolved.is_empty());
        assert_eq!(graph.tasks[0].status, TaskStatus::Ready);
    }

    // --- reset_for_retry route_to re-arm tests (D2, spec-075 FR-D-01) ---

    #[test]
    fn test_reset_for_retry_rearms_dormant_fallback_that_never_ran() {
        // Source failed (never rerouted, target still Dormant); graph failed for an
        // unrelated reason. Retry must leave the (already-Dormant) target untouched.
        let mut graph = graph_from_nodes(make_route_to_pair());
        graph.tasks[0].status = TaskStatus::Dormant;
        graph.tasks[1].status = TaskStatus::Failed;
        graph.status = GraphStatus::Failed;

        let __ra = make_rev_adj(&graph);
        reset_for_retry(&mut graph, &__ra).unwrap();

        assert_eq!(
            graph.tasks[1].status,
            TaskStatus::Ready,
            "source reset for retry"
        );
        assert_eq!(graph.tasks[0].status, TaskStatus::Dormant);
        assert_eq!(graph.tasks[0].routed_from, None);
    }

    #[test]
    fn test_reset_for_retry_rearms_activated_fallback_case_a_source_now_succeeds() {
        // D2 case (a): F already ran (activated + Completed) from a prior reroute; the
        // graph later failed for an unrelated reason. On retry, S is reset to Ready and
        // F must be re-armed back to Dormant -- if S now succeeds, F must NOT dispatch.
        let mut graph = graph_from_nodes(make_route_to_pair());
        graph.tasks[0].status = TaskStatus::Completed; // F already ran
        graph.tasks[0].routed_from = Some(TaskId(1));
        graph.tasks[0].result = Some(TaskResult {
            output: "stale fallback output".to_string(),
            artifacts: Vec::new(),
            duration_ms: 5,
            agent_id: None,
            agent_def: None,
        });
        graph.tasks[1].status = TaskStatus::Failed; // S: the route_to source
        graph.status = GraphStatus::Failed;

        let __ra = make_rev_adj(&graph);
        reset_for_retry(&mut graph, &__ra).unwrap();

        assert_eq!(graph.tasks[1].status, TaskStatus::Ready);
        assert_eq!(
            graph.tasks[0].status,
            TaskStatus::Dormant,
            "activated fallback must be re-armed to Dormant on retry"
        );
        assert_eq!(
            graph.tasks[0].routed_from, None,
            "stale routed_from must be cleared"
        );
        assert!(
            graph.tasks[0].result.is_none(),
            "stale result must be cleared"
        );

        // S now succeeds -- F must stay parked, not dispatch.
        graph.tasks[1].status = TaskStatus::Completed;
        let ready = ready_tasks(&graph);
        assert!(
            !ready.contains(&TaskId(0)),
            "re-armed Dormant fallback must not dispatch when the source now succeeds"
        );
    }

    #[test]
    fn test_reset_for_retry_rearms_activated_fallback_case_b_source_fails_again() {
        // D2 case (b): same setup, but after retry S fails again -- try_reroute must
        // fire again (F was correctly re-Dormant, not stuck Ready/beyond).
        let mut graph = graph_from_nodes(make_route_to_pair());
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[0].routed_from = Some(TaskId(1));
        graph.tasks[1].status = TaskStatus::Failed;
        graph.status = GraphStatus::Failed;

        let __ra = make_rev_adj(&graph);
        reset_for_retry(&mut graph, &__ra).unwrap();
        assert_eq!(graph.tasks[0].status, TaskStatus::Dormant);

        // S fails again.
        graph.tasks[1].status = TaskStatus::Failed;
        graph.tasks[1].failure_strategy = Some(FailureStrategy::Abort);
        let to_cancel = propagate_failure(&mut graph, TaskId(1), &__ra);

        assert!(to_cancel.is_empty());
        assert_eq!(
            graph.tasks[0].status,
            TaskStatus::Ready,
            "reroute must fire again after the re-arm"
        );
        assert_eq!(graph.tasks[0].routed_from, Some(TaskId(1)));
    }

    #[test]
    fn test_reset_for_retry_rearm_resets_fallback_subtree() {
        // F(0) <- routed by S(1); G(2) depends_on F(0). A prior reroute ran F to
        // Completed and G to Completed too. Retry must walk F's subtree and reset G
        // back to Pending for a clean re-run.
        let mut graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[0]),
        ]);
        graph.tasks[1].recovery = Some(crate::graph::RecoveryAction {
            state_injection: None,
            route_to: Some(TaskId(0)),
        });
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[0].routed_from = Some(TaskId(1));
        graph.tasks[1].status = TaskStatus::Failed;
        graph.tasks[2].status = TaskStatus::Completed;
        graph.status = GraphStatus::Failed;

        let __ra = make_rev_adj(&graph);
        reset_for_retry(&mut graph, &__ra).unwrap();

        assert_eq!(graph.tasks[0].status, TaskStatus::Dormant);
        assert_eq!(
            graph.tasks[2].status,
            TaskStatus::Pending,
            "F's downstream subtree must reset to Pending alongside the re-arm"
        );
    }

    #[test]
    fn test_reset_for_retry_does_not_rearm_untouched_route_to_source() {
        // S succeeded (not in `seeds`); the graph failed for an unrelated reason. The
        // untouched source's fallback branch must not be re-armed.
        let mut graph = graph_from_nodes(vec![
            make_node(0, &[]), // F (unrelated route_to target)
            make_node(1, &[]), // S (succeeded)
            make_node(2, &[]), // unrelated failed task causing graph Failed
        ]);
        graph.tasks[1].recovery = Some(crate::graph::RecoveryAction {
            state_injection: None,
            route_to: Some(TaskId(0)),
        });
        graph.tasks[0].status = TaskStatus::Dormant;
        graph.tasks[1].status = TaskStatus::Completed; // S succeeded, never rerouted
        graph.tasks[2].status = TaskStatus::Failed;
        graph.status = GraphStatus::Failed;

        let __ra = make_rev_adj(&graph);
        reset_for_retry(&mut graph, &__ra).unwrap();

        assert_eq!(
            graph.tasks[1].status,
            TaskStatus::Completed,
            "S was not reset (not Failed)"
        );
        assert_eq!(
            graph.tasks[0].status,
            TaskStatus::Dormant,
            "F must be untouched since its source was never reset"
        );
    }

    // --- toposort tests ---

    #[test]
    fn test_toposort_linear() {
        let tasks = vec![make_node(0, &[]), make_node(1, &[0]), make_node(2, &[1])];
        let order = toposort(&tasks).expect("should succeed");
        assert_eq!(order, vec![TaskId(0), TaskId(1), TaskId(2)]);
    }

    #[test]
    fn test_toposort_diamond() {
        let tasks = vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[0]),
            make_node(3, &[1, 2]),
        ];
        let order = toposort(&tasks).expect("should succeed");
        // 0 must come first, 3 must come last
        assert_eq!(order[0], TaskId(0));
        assert_eq!(order[3], TaskId(3));
    }

    #[test]
    fn test_toposort_wide_parallel() {
        let tasks = vec![make_node(0, &[]), make_node(1, &[]), make_node(2, &[])];
        let order = toposort(&tasks).expect("should succeed");
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn test_toposort_single_node() {
        let tasks = vec![make_node(0, &[])];
        let order = toposort(&tasks).expect("should succeed");
        assert_eq!(order, vec![TaskId(0)]);
    }

    // --- ready_tasks tests ---

    #[test]
    fn test_ready_tasks_initial_roots() {
        let mut graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[0, 1]),
        ]);
        graph.tasks[0].status = TaskStatus::Pending;
        graph.tasks[1].status = TaskStatus::Pending;
        graph.tasks[2].status = TaskStatus::Pending;
        let ready = ready_tasks(&graph);
        assert!(ready.contains(&TaskId(0)));
        assert!(ready.contains(&TaskId(1)));
        assert!(!ready.contains(&TaskId(2)));
    }

    #[test]
    fn test_ready_tasks_after_completion() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[1].status = TaskStatus::Pending;
        let ready = ready_tasks(&graph);
        assert!(ready.contains(&TaskId(1)));
    }

    #[test]
    fn test_ready_tasks_skipped_does_not_unblock() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Skipped;
        graph.tasks[1].status = TaskStatus::Pending;
        let ready = ready_tasks(&graph);
        assert!(!ready.contains(&TaskId(1)));
    }

    #[test]
    fn test_ready_tasks_partial_deps_completed() {
        let mut graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[0, 1]),
        ]);
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[1].status = TaskStatus::Running;
        graph.tasks[2].status = TaskStatus::Pending;
        let ready = ready_tasks(&graph);
        assert!(!ready.contains(&TaskId(2)));
    }

    #[test]
    fn test_ready_tasks_all_terminal() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[1].status = TaskStatus::Completed;
        let ready = ready_tasks(&graph);
        assert!(ready.is_empty());
    }

    #[test]
    fn test_ready_tasks_already_ready_included() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Ready; // already set to Ready
        graph.tasks[1].status = TaskStatus::Pending;
        let ready = ready_tasks(&graph);
        // TaskId(0) is Ready so it should be returned
        assert!(ready.contains(&TaskId(0)));
    }

    // --- predicate gate tests ---

    #[test]
    fn test_ready_tasks_predicate_gate_blocks_downstream() {
        use crate::graph::VerifyPredicate;
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        // Task 0 completed but predicate not yet evaluated.
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[0].verify_predicate = Some(VerifyPredicate::Natural(
            "output must be non-empty".to_string(),
        ));
        graph.tasks[0].predicate_outcome = None;
        graph.tasks[1].status = TaskStatus::Pending;

        let ready = ready_tasks(&graph);
        assert!(
            !ready.contains(&TaskId(1)),
            "task 1 must be blocked by uncleared predicate on task 0"
        );
    }

    #[test]
    fn test_ready_tasks_predicate_gate_unblocks_on_pass() {
        use crate::graph::{PredicateOutcome, VerifyPredicate};
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[0].verify_predicate = Some(VerifyPredicate::Natural("criterion".to_string()));
        graph.tasks[0].predicate_outcome = Some(PredicateOutcome {
            passed: true,
            confidence: 0.9,
            reason: "ok".to_string(),
        });
        graph.tasks[1].status = TaskStatus::Pending;

        let ready = ready_tasks(&graph);
        assert!(
            ready.contains(&TaskId(1)),
            "task 1 must be unblocked when predicate passed"
        );
    }

    #[test]
    fn test_ready_tasks_predicate_gate_remains_closed_on_fail() {
        use crate::graph::{PredicateOutcome, VerifyPredicate};
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[0].verify_predicate = Some(VerifyPredicate::Natural("criterion".to_string()));
        graph.tasks[0].predicate_outcome = Some(PredicateOutcome {
            passed: false,
            confidence: 0.1,
            reason: "criterion not met".to_string(),
        });
        graph.tasks[1].status = TaskStatus::Pending;

        let ready = ready_tasks(&graph);
        assert!(
            !ready.contains(&TaskId(1)),
            "task 1 must remain blocked when predicate failed"
        );
    }

    #[test]
    fn test_ready_tasks_no_predicate_unblocks_normally() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[1].status = TaskStatus::Pending;

        let ready = ready_tasks(&graph);
        assert!(
            ready.contains(&TaskId(1)),
            "no predicate = gate always clear"
        );
    }

    // --- propagate_failure tests ---

    #[test]
    fn test_propagate_failure_abort() {
        let mut graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[0]),
        ]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[1].status = TaskStatus::Running;
        graph.tasks[2].status = TaskStatus::Pending;
        graph.default_failure_strategy = FailureStrategy::Abort;

        let __ra = make_rev_adj(&graph);

        let to_cancel = propagate_failure(&mut graph, TaskId(0), &__ra);
        assert_eq!(graph.status, GraphStatus::Failed);
        assert!(to_cancel.contains(&TaskId(1)));
        assert!(!to_cancel.contains(&TaskId(2)));
    }

    #[test]
    fn test_propagate_failure_skip_single() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[0].failure_strategy = Some(FailureStrategy::Skip);
        graph.tasks[1].status = TaskStatus::Pending;

        let __ra = make_rev_adj(&graph);

        let to_cancel = propagate_failure(&mut graph, TaskId(0), &__ra);
        assert!(to_cancel.is_empty());
        assert_eq!(graph.tasks[0].status, TaskStatus::Skipped);
        assert_eq!(graph.tasks[1].status, TaskStatus::Skipped);
    }

    #[test]
    fn test_propagate_failure_skip_transitive() {
        // A(0) -> B(1) -> C(2): A fails with Skip
        let mut graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[1]),
        ]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[0].failure_strategy = Some(FailureStrategy::Skip);
        graph.tasks[1].status = TaskStatus::Pending;
        graph.tasks[2].status = TaskStatus::Pending;

        let __ra = make_rev_adj(&graph);

        propagate_failure(&mut graph, TaskId(0), &__ra);
        assert_eq!(graph.tasks[0].status, TaskStatus::Skipped);
        assert_eq!(graph.tasks[1].status, TaskStatus::Skipped);
        assert_eq!(graph.tasks[2].status, TaskStatus::Skipped);
    }

    #[test]
    fn test_propagate_failure_skip_running_dependent_returned() {
        // A(0) fails with Skip; B(1) is Running (actively executing)
        // The caller must cancel B — it cannot be stopped by just marking it Skipped
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[0].failure_strategy = Some(FailureStrategy::Skip);
        graph.tasks[1].status = TaskStatus::Running;

        let __ra = make_rev_adj(&graph);

        let to_cancel = propagate_failure(&mut graph, TaskId(0), &__ra);
        assert!(
            to_cancel.contains(&TaskId(1)),
            "Running dependent must be returned for cancellation"
        );
        assert_eq!(graph.tasks[1].status, TaskStatus::Skipped);
    }

    #[test]
    fn test_propagate_failure_retry_under_max() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[0].failure_strategy = Some(FailureStrategy::Retry);
        graph.tasks[0].max_retries = Some(3);
        graph.tasks[0].retry_count = 1;

        let __ra = make_rev_adj(&graph);

        let to_cancel = propagate_failure(&mut graph, TaskId(0), &__ra);
        assert!(to_cancel.is_empty());
        assert_eq!(graph.tasks[0].status, TaskStatus::Ready);
        assert_eq!(graph.tasks[0].retry_count, 2);
    }

    #[test]
    fn test_propagate_failure_retry_exhausted() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[0].failure_strategy = Some(FailureStrategy::Retry);
        graph.tasks[0].max_retries = Some(3);
        graph.tasks[0].retry_count = 3; // at max

        let __ra = make_rev_adj(&graph);

        propagate_failure(&mut graph, TaskId(0), &__ra);
        assert_eq!(graph.status, GraphStatus::Failed);
    }

    #[test]
    fn test_propagate_failure_ask() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[0].failure_strategy = Some(FailureStrategy::Ask);

        let __ra = make_rev_adj(&graph);

        let to_cancel = propagate_failure(&mut graph, TaskId(0), &__ra);
        assert!(to_cancel.is_empty());
        assert_eq!(graph.status, GraphStatus::Paused);
    }

    #[test]
    fn test_propagate_failure_per_task_override() {
        // Graph default is Abort, but task overrides with Skip
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.default_failure_strategy = FailureStrategy::Abort;
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[0].failure_strategy = Some(FailureStrategy::Skip);
        graph.tasks[1].status = TaskStatus::Pending;

        let __ra = make_rev_adj(&graph);

        propagate_failure(&mut graph, TaskId(0), &__ra);
        // Should use Skip, not Abort
        assert_eq!(graph.tasks[0].status, TaskStatus::Skipped);
        assert_ne!(graph.status, GraphStatus::Failed);
    }

    #[test]
    fn test_propagate_failure_already_terminal() {
        // Calling propagate_failure on a Completed task should be a no-op
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Completed;

        let __ra = make_rev_adj(&graph);

        let to_cancel = propagate_failure(&mut graph, TaskId(0), &__ra);
        assert!(to_cancel.is_empty());
        assert_eq!(graph.status, GraphStatus::Created);
    }

    // --- Mode-1 recovery tests ---

    #[test]
    fn test_propagate_failure_abort_recovers_with_state_injection() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.status = GraphStatus::Running;
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[0].failure_strategy = Some(FailureStrategy::Abort);
        graph.tasks[0].recovery = Some(crate::graph::RecoveryAction {
            state_injection: Some("fallback output".to_string()),
            route_to: None,
        });

        let __ra = make_rev_adj(&graph);

        let to_cancel = propagate_failure(&mut graph, TaskId(0), &__ra);
        assert!(to_cancel.is_empty());
        assert_eq!(graph.tasks[0].status, TaskStatus::Completed);
        assert_eq!(
            graph.tasks[0].result.as_ref().unwrap().output,
            "fallback output"
        );
        assert_eq!(
            graph.tasks[0].result.as_ref().unwrap().agent_def.as_deref(),
            Some("__recovery__")
        );
        assert_eq!(
            graph.status,
            GraphStatus::Running,
            "graph.status must be left untouched by recovery"
        );
    }

    #[test]
    fn test_propagate_failure_retry_exhausted_recovers_with_state_injection() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.status = GraphStatus::Running;
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[0].failure_strategy = Some(FailureStrategy::Retry);
        graph.tasks[0].max_retries = Some(3);
        graph.tasks[0].retry_count = 3; // at max — exhausted
        graph.tasks[0].recovery = Some(crate::graph::RecoveryAction {
            state_injection: Some("fallback output".to_string()),
            route_to: None,
        });

        let __ra = make_rev_adj(&graph);

        let to_cancel = propagate_failure(&mut graph, TaskId(0), &__ra);
        assert!(to_cancel.is_empty());
        assert_eq!(graph.tasks[0].status, TaskStatus::Completed);
        assert_eq!(
            graph.tasks[0].result.as_ref().unwrap().output,
            "fallback output"
        );
        assert_eq!(graph.status, GraphStatus::Running);
    }

    #[test]
    fn test_propagate_failure_abort_no_recovery_configured_is_unchanged() {
        // regression: recovery == None on both paths behaves byte-identical to pre-feature
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[0].failure_strategy = Some(FailureStrategy::Abort);

        let __ra = make_rev_adj(&graph);

        propagate_failure(&mut graph, TaskId(0), &__ra);
        assert_eq!(graph.status, GraphStatus::Failed);
        assert_eq!(graph.tasks[0].status, TaskStatus::Failed);
    }

    #[test]
    fn test_propagate_failure_retry_exhausted_no_recovery_configured_is_unchanged() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[0].failure_strategy = Some(FailureStrategy::Retry);
        graph.tasks[0].max_retries = Some(3);
        graph.tasks[0].retry_count = 3;

        let __ra = make_rev_adj(&graph);

        propagate_failure(&mut graph, TaskId(0), &__ra);
        assert_eq!(graph.status, GraphStatus::Failed);
    }

    #[test]
    fn test_recovered_task_dependent_becomes_ready() {
        // A(0) -> B(1): A fails (Abort) with recovery configured; B must unblock.
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.status = GraphStatus::Running;
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[0].failure_strategy = Some(FailureStrategy::Abort);
        graph.tasks[0].recovery = Some(crate::graph::RecoveryAction {
            state_injection: Some("fallback output".to_string()),
            route_to: None,
        });
        graph.tasks[1].status = TaskStatus::Pending;

        let __ra = make_rev_adj(&graph);

        propagate_failure(&mut graph, TaskId(0), &__ra);
        assert_eq!(graph.tasks[0].status, TaskStatus::Completed);

        let ready = ready_tasks(&graph);
        assert!(
            ready.contains(&TaskId(1)),
            "dependent must unblock via the Pending arm after recovery"
        );
    }

    #[test]
    fn test_skip_strategy_with_recovery_configured_still_skips() {
        // recovery configured but effective strategy is Skip — recovery must never be
        // consulted from the Skip arm; the task ends Skipped, not Completed.
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[0].failure_strategy = Some(FailureStrategy::Skip);
        graph.tasks[0].recovery = Some(crate::graph::RecoveryAction {
            state_injection: Some("fallback output".to_string()),
            route_to: None,
        });

        let __ra = make_rev_adj(&graph);

        propagate_failure(&mut graph, TaskId(0), &__ra);
        assert_eq!(graph.tasks[0].status, TaskStatus::Skipped);
    }

    #[test]
    fn test_ask_strategy_with_recovery_configured_still_pauses() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[0].failure_strategy = Some(FailureStrategy::Ask);
        graph.tasks[0].recovery = Some(crate::graph::RecoveryAction {
            state_injection: Some("fallback output".to_string()),
            route_to: None,
        });

        let __ra = make_rev_adj(&graph);

        propagate_failure(&mut graph, TaskId(0), &__ra);
        assert_eq!(graph.status, GraphStatus::Paused);
        assert_ne!(graph.tasks[0].status, TaskStatus::Completed);
    }

    // --- reset_for_retry tests ---

    #[test]
    fn test_reset_for_retry_resets_failed_to_ready() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.status = GraphStatus::Failed;

        let __ra = make_rev_adj(&graph);

        reset_for_retry(&mut graph, &__ra).unwrap();
        assert_eq!(graph.tasks[0].status, TaskStatus::Ready);
        assert_eq!(graph.status, GraphStatus::Running);
    }

    #[test]
    fn test_reset_for_retry_resets_skipped_dependents_to_pending() {
        // A(0) -> B(1): A fails, B skipped. After retry, B should be Pending again.
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[1].status = TaskStatus::Skipped;
        graph.status = GraphStatus::Failed;

        let __ra = make_rev_adj(&graph);

        reset_for_retry(&mut graph, &__ra).unwrap();
        assert_eq!(graph.tasks[0].status, TaskStatus::Ready);
        assert_eq!(graph.tasks[1].status, TaskStatus::Pending);
    }

    #[test]
    fn test_reset_for_retry_transitive_skipped_reset() {
        // A(0) -> B(1) -> C(2): A fails, B and C skipped. All skipped reset to Pending.
        let mut graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[1]),
        ]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[1].status = TaskStatus::Skipped;
        graph.tasks[2].status = TaskStatus::Skipped;
        graph.status = GraphStatus::Failed;

        let __ra = make_rev_adj(&graph);

        reset_for_retry(&mut graph, &__ra).unwrap();
        assert_eq!(graph.tasks[0].status, TaskStatus::Ready);
        assert_eq!(graph.tasks[1].status, TaskStatus::Pending);
        assert_eq!(graph.tasks[2].status, TaskStatus::Pending);
    }

    #[test]
    fn test_reset_for_retry_completed_tasks_unchanged() {
        // Only failed/skipped tasks should be touched; completed tasks stay completed.
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[1].status = TaskStatus::Failed;
        graph.status = GraphStatus::Failed;

        let __ra = make_rev_adj(&graph);

        reset_for_retry(&mut graph, &__ra).unwrap();
        assert_eq!(graph.tasks[0].status, TaskStatus::Completed);
        assert_eq!(graph.tasks[1].status, TaskStatus::Ready);
    }

    #[test]
    fn test_reset_for_retry_rejects_running_graph() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Running;
        graph.status = GraphStatus::Running;

        let __ra = make_rev_adj(&graph);

        let err = reset_for_retry(&mut graph, &__ra).unwrap_err();
        assert_matches!(err, OrchestrationError::InvalidGraph(_));
    }

    #[test]
    fn test_reset_for_retry_paused_graph_ok() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[1].status = TaskStatus::Skipped;
        graph.status = GraphStatus::Paused;

        let __ra = make_rev_adj(&graph);

        reset_for_retry(&mut graph, &__ra).unwrap();
        assert_eq!(graph.status, GraphStatus::Running);
    }

    #[test]
    fn test_reset_for_retry_clears_retry_count() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[0].retry_count = 5;
        graph.status = GraphStatus::Failed;

        let __ra = make_rev_adj(&graph);

        reset_for_retry(&mut graph, &__ra).unwrap();
        assert_eq!(graph.tasks[0].retry_count, 0);
    }

    #[test]
    fn test_reset_for_retry_paused_no_failed_tasks() {
        // Paused graph with no failed tasks (e.g. user paused manually)
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Completed;
        graph.status = GraphStatus::Paused;

        let __ra = make_rev_adj(&graph);

        reset_for_retry(&mut graph, &__ra).unwrap();
        assert_eq!(graph.status, GraphStatus::Running);
        assert_eq!(graph.tasks[0].status, TaskStatus::Completed);
    }

    #[test]
    fn test_reset_for_retry_canceled_tasks_reset_to_pending() {
        // IC2: after Abort cascade, running tasks are Canceled. They must be reset
        // to Pending so their dependents can be re-evaluated.
        let mut graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[0, 1]),
        ]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[1].status = TaskStatus::Canceled; // was Running, aborted
        graph.tasks[2].status = TaskStatus::Pending;
        graph.status = GraphStatus::Failed;

        let __ra = make_rev_adj(&graph);

        reset_for_retry(&mut graph, &__ra).unwrap();
        assert_eq!(graph.tasks[0].status, TaskStatus::Ready);
        assert_eq!(
            graph.tasks[1].status,
            TaskStatus::Pending,
            "Canceled task must be reset to Pending (IC2)"
        );
        assert_eq!(graph.tasks[2].status, TaskStatus::Pending);
    }

    #[test]
    fn test_reset_for_retry_canceled_unblocks_dependents() {
        // A(0) -> B(1): A fails, B was Running (Canceled after Abort).
        // After retry B should be Pending so ready_tasks() can pick it up.
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[1].status = TaskStatus::Canceled;
        graph.status = GraphStatus::Failed;

        let __ra = make_rev_adj(&graph);

        reset_for_retry(&mut graph, &__ra).unwrap();
        assert_eq!(graph.tasks[0].status, TaskStatus::Ready);
        assert_eq!(graph.tasks[1].status, TaskStatus::Pending);
    }

    // --- lookahead_tools tests ---

    fn make_node_titled(id: u32, deps: &[u32], title: &str, desc: &str) -> TaskNode {
        let mut n = TaskNode::new(id, title, desc);
        n.depends_on = deps.iter().map(|&d| TaskId(d)).collect();
        n
    }

    #[test]
    fn lookahead_depth_zero_returns_empty() {
        let mut graph = graph_from_nodes(vec![
            make_node_titled(0, &[], "web_search", "Search the web for results"),
            make_node_titled(1, &[0], "summarize", "Summarize findings"),
        ]);
        graph.tasks[0].status = TaskStatus::Running;
        graph.tasks[1].status = TaskStatus::Pending;

        let hints = lookahead_tools(&graph, 0);
        assert!(hints.is_empty(), "depth=0 must return empty vec");
    }

    #[test]
    fn lookahead_depth_one_emits_only_direct_child() {
        // A(0, Running) -> B(1, Pending, tool: web_search) -> C(2, Pending, tool: summarize)
        let mut graph = graph_from_nodes(vec![
            make_node_titled(0, &[], "task-a", "Root task"),
            make_node_titled(1, &[0], "web_search", "Search the web"),
            make_node_titled(2, &[1], "summarize", "Summarize search results"),
        ]);
        graph.tasks[0].status = TaskStatus::Running;
        graph.tasks[1].status = TaskStatus::Pending;
        graph.tasks[2].status = TaskStatus::Pending;

        let hints = lookahead_tools(&graph, 1);
        assert_eq!(hints.len(), 1, "depth=1 should emit only B");
        assert_eq!(hints[0].tool_name, "web_search");
        assert_eq!(hints[0].distance_from_current, 1);
    }

    #[test]
    fn lookahead_depth_two_emits_both_children() {
        // A(0, Running) -> B(1, Pending) -> C(2, Pending)
        let mut graph = graph_from_nodes(vec![
            make_node_titled(0, &[], "task-a", "Root task"),
            make_node_titled(1, &[0], "web_search", "Search the web"),
            make_node_titled(2, &[1], "summarize", "Summarize search results"),
        ]);
        graph.tasks[0].status = TaskStatus::Running;
        graph.tasks[1].status = TaskStatus::Pending;
        graph.tasks[2].status = TaskStatus::Pending;

        let hints = lookahead_tools(&graph, 2);
        assert_eq!(hints.len(), 2, "depth=2 should emit B and C");
        assert_eq!(hints[0].tool_name, "web_search");
        assert_eq!(hints[0].distance_from_current, 1);
        assert_eq!(hints[1].tool_name, "summarize");
        assert_eq!(hints[1].distance_from_current, 2);
    }

    #[test]
    fn lookahead_no_frontier_returns_empty() {
        // All tasks are Pending — no Running or Ready frontier
        let mut graph = graph_from_nodes(vec![
            make_node_titled(0, &[], "task-a", "Root"),
            make_node_titled(1, &[0], "task-b", "Child"),
        ]);
        graph.tasks[0].status = TaskStatus::Pending;
        graph.tasks[1].status = TaskStatus::Pending;

        let hints = lookahead_tools(&graph, 2);
        assert!(hints.is_empty(), "no frontier → empty");
    }

    #[test]
    fn lookahead_frontier_not_emitted() {
        // Running task itself must NOT appear in output
        let mut graph = graph_from_nodes(vec![
            make_node_titled(0, &[], "running-tool", "Currently executing"),
            make_node_titled(1, &[0], "next-tool", "Next step"),
        ]);
        graph.tasks[0].status = TaskStatus::Running;
        graph.tasks[1].status = TaskStatus::Pending;

        let hints = lookahead_tools(&graph, 3);
        assert!(
            hints.iter().all(|h| h.tool_name != "running-tool"),
            "frontier task must not be emitted"
        );
        assert_eq!(hints.len(), 1);
    }

    #[test]
    fn lookahead_uses_agent_hint_as_tool_name() {
        let mut graph = graph_from_nodes(vec![
            make_node_titled(0, &[], "dispatch", "Root"),
            make_node_titled(1, &[0], "raw-title", "Execute shell command"),
        ]);
        graph.tasks[0].status = TaskStatus::Running;
        graph.tasks[1].status = TaskStatus::Pending;
        graph.tasks[1].agent_hint = Some("shell_executor".to_string());

        let hints = lookahead_tools(&graph, 1);
        assert_eq!(hints.len(), 1);
        assert_eq!(
            hints[0].tool_name, "shell_executor",
            "agent_hint should take precedence over title"
        );
    }

    #[test]
    fn lookahead_results_sorted_by_distance() {
        // A(0, Running) -> B(1) and B(1) -> C(2): should be sorted 1, 2
        let mut graph = graph_from_nodes(vec![
            make_node_titled(0, &[], "root", "Root"),
            make_node_titled(1, &[0], "step-one", "Step one"),
            make_node_titled(2, &[1], "step-two", "Step two"),
        ]);
        graph.tasks[0].status = TaskStatus::Running;
        graph.tasks[1].status = TaskStatus::Pending;
        graph.tasks[2].status = TaskStatus::Pending;

        let hints = lookahead_tools(&graph, 2);
        for w in hints.windows(2) {
            assert!(
                w[0].distance_from_current <= w[1].distance_from_current,
                "hints must be sorted by distance"
            );
        }
    }

    #[test]
    fn lookahead_keywords_extracted_and_deduped() {
        let mut graph = graph_from_nodes(vec![
            make_node_titled(0, &[], "root", "Root task"),
            make_node_titled(1, &[0], "search", "search search search results web"),
        ]);
        graph.tasks[0].status = TaskStatus::Running;
        graph.tasks[1].status = TaskStatus::Pending;

        let hints = lookahead_tools(&graph, 1);
        assert_eq!(hints.len(), 1);
        // "search" appears multiple times but should be deduped to one entry
        let count = hints[0]
            .keywords
            .iter()
            .filter(|k| k.as_str() == "search")
            .count();
        assert_eq!(count, 1, "duplicate keywords must be deduplicated");
    }

    #[test]
    fn lookahead_stopwords_filtered() {
        let mut graph = graph_from_nodes(vec![
            make_node_titled(0, &[], "root", "Root"),
            make_node_titled(
                1,
                &[0],
                "task",
                "the result of the operation from the source",
            ),
        ]);
        graph.tasks[0].status = TaskStatus::Running;
        graph.tasks[1].status = TaskStatus::Pending;

        let hints = lookahead_tools(&graph, 1);
        assert_eq!(hints.len(), 1);
        for kw in &hints[0].keywords {
            assert!(
                !KEYWORD_STOPWORDS.contains(&kw.as_str()),
                "stopword '{kw}' must not appear in keywords"
            );
        }
    }
}
