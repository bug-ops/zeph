// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::VecDeque;

use super::error::OrchestrationError;
use super::graph::{FailureStrategy, GraphStatus, TaskGraph, TaskId, TaskNode, TaskStatus};

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
///
/// # Errors
///
/// Returns `OrchestrationError::InvalidGraph` for structural violations,
/// or `OrchestrationError::CycleDetected` if a cycle is found.
pub fn validate(tasks: &[TaskNode], max_tasks: usize) -> Result<(), OrchestrationError> {
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
    }

    // Cycle detection + root check via toposort
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

/// Find tasks that are ready to be scheduled.
///
/// Returns tasks that are either:
/// - In `Ready` status (already marked ready but not yet running), or
/// - In `Pending` status with all dependencies in `Completed` state.
///
/// This makes the function idempotent across scheduler ticks.
#[must_use]
pub fn ready_tasks(graph: &TaskGraph) -> Vec<TaskId> {
    graph
        .tasks
        .iter()
        .filter_map(|task| {
            match task.status {
                TaskStatus::Ready => Some(task.id),
                TaskStatus::Pending => {
                    // All deps must be Completed to unblock
                    let all_deps_done = task
                        .depends_on
                        .iter()
                        .all(|dep_id| graph.tasks[dep_id.index()].status == TaskStatus::Completed);
                    if all_deps_done { Some(task.id) } else { None }
                }
                _ => None,
            }
        })
        .collect()
}

/// Handle a task failure. Applies the effective failure strategy and mutates the graph.
///
/// Returns the list of `Running` task IDs that the caller should cancel (for `Abort` strategy).
///
/// - `Abort`: sets `graph.status = Failed`, returns all currently `Running` task IDs.
/// - `Skip`: marks the failed task `Skipped` and transitively skips all non-terminal dependents
///   using BFS over a reverse adjacency list.
/// - `Retry`: if `retry_count < max_retries`, increments counter and resets task to `Ready`.
///   Otherwise falls through to `Abort`.
/// - `Ask`: sets `graph.status = Paused`.
pub fn propagate_failure(graph: &mut TaskGraph, failed_id: TaskId) -> Vec<TaskId> {
    let task_count = graph.tasks.len();

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
            // Mark the failed task as Skipped
            graph.tasks[failed_id.index()].status = TaskStatus::Skipped;

            // Build reverse adjacency list
            let mut dependents: Vec<Vec<TaskId>> = vec![Vec::new(); task_count];
            for task in &graph.tasks {
                for dep in &task.depends_on {
                    dependents[dep.index()].push(task.id);
                }
            }

            // BFS to transitively skip all non-terminal dependents.
            // Collect Running tasks that are being skipped — the caller must cancel them,
            // because marking a task Skipped in the data structure does not stop execution.
            let mut to_cancel = Vec::new();
            let mut queue: VecDeque<TaskId> = VecDeque::new();
            queue.push_back(failed_id);

            while let Some(current) = queue.pop_front() {
                for &dep_id in &dependents[current.index()] {
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

        FailureStrategy::Retry => {
            let retry_count = graph.tasks[failed_id.index()].retry_count;
            if retry_count < max_retries {
                graph.tasks[failed_id.index()].retry_count += 1;
                graph.tasks[failed_id.index()].status = TaskStatus::Ready;
                Vec::new()
            } else {
                // Retry exhausted — treat as Abort
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
/// # Errors
///
/// Returns `OrchestrationError::InvalidGraph` if the graph is not in `Failed`
/// or `Paused` status (the only states that make sense to retry from).
pub fn reset_for_retry(graph: &mut TaskGraph) -> Result<(), OrchestrationError> {
    use super::graph::GraphStatus;

    if graph.status != GraphStatus::Failed && graph.status != GraphStatus::Paused {
        return Err(OrchestrationError::InvalidGraph(format!(
            "cannot retry graph in status {}; only Failed or Paused graphs can be retried",
            graph.status
        )));
    }

    let task_count = graph.tasks.len();

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

    // Build reverse adjacency: dependents[i] = tasks that depend on task i.
    let mut dependents: Vec<Vec<TaskId>> = vec![Vec::new(); task_count];
    for task in &graph.tasks {
        for dep in &task.depends_on {
            dependents[dep.index()].push(task.id);
        }
    }

    // BFS from seeds: reset Skipped dependents back to Pending.
    let mut queue: std::collections::VecDeque<TaskId> = seeds.into_iter().collect();
    while let Some(current) = queue.pop_front() {
        for &dep_id in &dependents[current.index()] {
            if graph.tasks[dep_id.index()].status == TaskStatus::Skipped {
                graph.tasks[dep_id.index()].status = TaskStatus::Pending;
                queue.push_back(dep_id);
            }
        }
    }

    graph.status = GraphStatus::Running;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{FailureStrategy, GraphStatus, TaskGraph, TaskNode, TaskStatus};

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

    // --- validate tests ---

    #[test]
    fn test_validate_empty_graph() {
        let err = validate(&[], 20).unwrap_err();
        assert!(matches!(err, OrchestrationError::InvalidGraph(_)));
    }

    #[test]
    fn test_validate_exceeds_max_tasks() {
        let tasks: Vec<TaskNode> = (0..5).map(|i| make_node(i, &[])).collect();
        let err = validate(&tasks, 3).unwrap_err();
        assert!(matches!(err, OrchestrationError::InvalidGraph(_)));
    }

    #[test]
    fn test_validate_single_task_no_deps() {
        let tasks = vec![make_node(0, &[])];
        assert!(validate(&tasks, 20).is_ok());
    }

    #[test]
    fn test_validate_self_reference() {
        let mut tasks = vec![make_node(0, &[])];
        tasks[0].depends_on = vec![TaskId(0)];
        let err = validate(&tasks, 20).unwrap_err();
        assert!(matches!(err, OrchestrationError::InvalidGraph(_)));
    }

    #[test]
    fn test_validate_invalid_taskid_reference() {
        let mut tasks = vec![make_node(0, &[])];
        tasks[0].depends_on = vec![TaskId(99)];
        let err = validate(&tasks, 20).unwrap_err();
        assert!(matches!(err, OrchestrationError::InvalidGraph(_)));
    }

    #[test]
    fn test_validate_linear_chain() {
        // A(0) -> B(1) -> C(2)
        let tasks = vec![make_node(0, &[]), make_node(1, &[0]), make_node(2, &[1])];
        assert!(validate(&tasks, 20).is_ok());
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
        assert!(validate(&tasks, 20).is_ok());
    }

    #[test]
    fn test_validate_cycle_two_nodes() {
        // A(0) depends on B(1), B(1) depends on A(0)
        let tasks = vec![make_node(0, &[1]), make_node(1, &[0])];
        let err = validate(&tasks, 20).unwrap_err();
        assert!(matches!(err, OrchestrationError::CycleDetected));
    }

    #[test]
    fn test_validate_cycle_three_nodes() {
        // A(0)->B(1)->C(2)->A(0)
        let tasks = vec![make_node(0, &[2]), make_node(1, &[0]), make_node(2, &[1])];
        let err = validate(&tasks, 20).unwrap_err();
        assert!(matches!(err, OrchestrationError::CycleDetected));
    }

    #[test]
    fn test_validate_taskid_invariant() {
        let mut tasks = vec![make_node(0, &[]), make_node(1, &[0])];
        // Break invariant: tasks[1] should have id TaskId(1) but we set TaskId(5)
        tasks[1].id = TaskId(5);
        let err = validate(&tasks, 20).unwrap_err();
        assert!(matches!(err, OrchestrationError::InvalidGraph(_)));
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

        let to_cancel = propagate_failure(&mut graph, TaskId(0));
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

        let to_cancel = propagate_failure(&mut graph, TaskId(0));
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

        propagate_failure(&mut graph, TaskId(0));
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

        let to_cancel = propagate_failure(&mut graph, TaskId(0));
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

        let to_cancel = propagate_failure(&mut graph, TaskId(0));
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

        propagate_failure(&mut graph, TaskId(0));
        assert_eq!(graph.status, GraphStatus::Failed);
    }

    #[test]
    fn test_propagate_failure_ask() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[0].failure_strategy = Some(FailureStrategy::Ask);

        let to_cancel = propagate_failure(&mut graph, TaskId(0));
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

        propagate_failure(&mut graph, TaskId(0));
        // Should use Skip, not Abort
        assert_eq!(graph.tasks[0].status, TaskStatus::Skipped);
        assert_ne!(graph.status, GraphStatus::Failed);
    }

    #[test]
    fn test_propagate_failure_already_terminal() {
        // Calling propagate_failure on a Completed task should be a no-op
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Completed;

        let to_cancel = propagate_failure(&mut graph, TaskId(0));
        assert!(to_cancel.is_empty());
        assert_eq!(graph.status, GraphStatus::Created);
    }

    // --- reset_for_retry tests ---

    #[test]
    fn test_reset_for_retry_resets_failed_to_ready() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.status = GraphStatus::Failed;

        reset_for_retry(&mut graph).unwrap();
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

        reset_for_retry(&mut graph).unwrap();
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

        reset_for_retry(&mut graph).unwrap();
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

        reset_for_retry(&mut graph).unwrap();
        assert_eq!(graph.tasks[0].status, TaskStatus::Completed);
        assert_eq!(graph.tasks[1].status, TaskStatus::Ready);
    }

    #[test]
    fn test_reset_for_retry_rejects_running_graph() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Running;
        graph.status = GraphStatus::Running;

        let err = reset_for_retry(&mut graph).unwrap_err();
        assert!(matches!(err, OrchestrationError::InvalidGraph(_)));
    }

    #[test]
    fn test_reset_for_retry_paused_graph_ok() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[1].status = TaskStatus::Skipped;
        graph.status = GraphStatus::Paused;

        reset_for_retry(&mut graph).unwrap();
        assert_eq!(graph.status, GraphStatus::Running);
    }

    #[test]
    fn test_reset_for_retry_clears_retry_count() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[0].retry_count = 5;
        graph.status = GraphStatus::Failed;

        reset_for_retry(&mut graph).unwrap();
        assert_eq!(graph.tasks[0].retry_count, 0);
    }

    #[test]
    fn test_reset_for_retry_paused_no_failed_tasks() {
        // Paused graph with no failed tasks (e.g. user paused manually)
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Completed;
        graph.status = GraphStatus::Paused;

        reset_for_retry(&mut graph).unwrap();
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

        reset_for_retry(&mut graph).unwrap();
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

        reset_for_retry(&mut graph).unwrap();
        assert_eq!(graph.tasks[0].status, TaskStatus::Ready);
        assert_eq!(graph.tasks[1].status, TaskStatus::Pending);
    }
}
