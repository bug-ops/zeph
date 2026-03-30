// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Cascade-aware routing for DAG execution (arXiv:2603.17112).
//!
//! Tracks failure propagation across DAG regions. When a subtree's failure rate
//! exceeds the configured threshold, tasks in that subtree are deprioritized in
//! `DagScheduler::tick()` so that healthy independent branches run first.

use std::collections::{HashMap, HashSet};

use super::graph::{TaskGraph, TaskId};

/// Per-region failure health snapshot.
#[derive(Debug, Clone)]
pub struct RegionHealth {
    pub total_tasks: usize,
    pub failed_tasks: usize,
    /// `failed_tasks / total_tasks`. NaN when `total_tasks = 0`.
    pub failure_rate: f32,
}

impl RegionHealth {
    fn new() -> Self {
        Self {
            total_tasks: 0,
            failed_tasks: 0,
            failure_rate: 0.0,
        }
    }

    fn record(&mut self, failed: bool) {
        self.total_tasks += 1;
        if failed {
            self.failed_tasks += 1;
        }
        #[allow(clippy::cast_precision_loss)]
        {
            self.failure_rate = self.failed_tasks as f32 / self.total_tasks as f32;
        }
    }
}

/// Configuration for cascade detection. Extracted from `OrchestrationConfig` at construction.
#[derive(Debug, Clone)]
pub struct CascadeConfig {
    /// Failure rate threshold above which a region is considered "cascading".
    pub failure_threshold: f32,
}

/// Tracks failure propagation across DAG regions for cascade-aware routing.
///
/// A "region" is the set of tasks reachable from the "heaviest root" of each task — the
/// root ancestor that is the source of the most downstream tasks. For tasks with multiple
/// roots (diamond patterns), we pick the single root that covers the most descendants,
/// preventing over-aggressive deprioritisation (C8 fix).
#[derive(Debug)]
pub struct CascadeDetector {
    config: CascadeConfig,
    /// Per-root failure health. Key = root `TaskId`.
    region_health: HashMap<TaskId, RegionHealth>,
}

impl CascadeDetector {
    /// Create a new detector.
    #[must_use]
    pub fn new(config: CascadeConfig) -> Self {
        Self {
            config,
            region_health: HashMap::new(),
        }
    }

    /// Record a task outcome and update the health of its primary region.
    pub fn record_outcome(&mut self, task_id: TaskId, succeeded: bool, graph: &TaskGraph) {
        let root = primary_root(task_id, graph);
        self.region_health
            .entry(root)
            .or_insert_with(RegionHealth::new)
            .record(!succeeded);
    }

    /// Returns `true` when the primary region of `task_id` is in cascade failure.
    #[must_use]
    pub fn is_cascading(&self, task_id: TaskId, graph: &TaskGraph) -> bool {
        let root = primary_root(task_id, graph);
        self.region_health
            .get(&root)
            .is_some_and(|h| h.failure_rate > self.config.failure_threshold)
    }

    /// Returns the set of task IDs that should be deprioritized due to cascade failure.
    ///
    /// Returns an empty set when no region is cascading, avoiding unnecessary reordering.
    #[must_use]
    pub fn deprioritized_tasks(&self, graph: &TaskGraph) -> HashSet<TaskId> {
        // Collect cascading roots first to avoid calling is_cascading per-task.
        let cascading_roots: HashSet<TaskId> = self
            .region_health
            .iter()
            .filter(|(_, h)| h.failure_rate > self.config.failure_threshold)
            .map(|(&root, _)| root)
            .collect();

        if cascading_roots.is_empty() {
            return HashSet::new();
        }

        // Log degenerate case: all known regions are cascading.
        let total_regions = self.region_health.len();
        if cascading_roots.len() == total_regions && total_regions > 0 {
            tracing::warn!(
                cascading_regions = total_regions,
                "all DAG regions are in cascade failure state; \
                 deprioritisation has no effect — falling back to default ordering"
            );
            return HashSet::new();
        }

        graph
            .tasks
            .iter()
            .filter(|t| cascading_roots.contains(&primary_root(t.id, graph)))
            .map(|t| t.id)
            .collect()
    }

    /// Reset all region health counters.
    ///
    /// Called by `DagScheduler::inject_tasks()` because graph topology has fundamentally
    /// changed — old failure counts no longer reflect the new task set (C13 fix).
    pub fn reset(&mut self) {
        self.region_health.clear();
    }

    /// Expose region health for testing.
    #[cfg(test)]
    #[must_use]
    pub fn region_health(&self) -> &HashMap<TaskId, RegionHealth> {
        &self.region_health
    }
}

/// Compute the "heaviest" root for `task_id`: the root ancestor that reaches the most
/// downstream tasks (largest subtree). For tasks that have no ancestors (roots themselves)
/// `task_id` is returned directly.
///
/// "Heaviest root" prevents over-aggressive cascade deprioritisation on diamond DAGs: if
/// task C is reachable from both A and B, we assign it to whichever root's subtree is
/// larger. Ties are broken by smaller `TaskId` value for determinism.
fn primary_root(task_id: TaskId, graph: &TaskGraph) -> TaskId {
    let roots = ancestor_roots(task_id, graph);
    if roots.is_empty() {
        return task_id;
    }
    if roots.len() == 1 {
        return roots[0];
    }

    // Count descendants for each root candidate.
    roots
        .into_iter()
        .max_by_key(|&r| (descendant_count(r, graph), u32::MAX - r.as_u32()))
        .unwrap_or(task_id)
}

/// Collect all root (in-degree 0) ancestors of `task_id` via BFS.
fn ancestor_roots(task_id: TaskId, graph: &TaskGraph) -> Vec<TaskId> {
    let mut visited = HashSet::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(task_id);
    visited.insert(task_id);

    let mut roots = Vec::new();

    while let Some(id) = queue.pop_front() {
        let task = &graph.tasks[id.index()];
        if task.depends_on.is_empty() {
            roots.push(id);
        } else {
            for &dep in &task.depends_on {
                if visited.insert(dep) {
                    queue.push_back(dep);
                }
            }
        }
    }

    roots
}

/// Count the number of tasks reachable from `root` (inclusive) via BFS.
fn descendant_count(root: TaskId, graph: &TaskGraph) -> usize {
    let mut visited = HashSet::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(root);
    visited.insert(root);

    // Build forward adjacency on the fly.
    // Tasks store `depends_on` (reverse edges). We need forward edges.
    let mut forward: HashMap<TaskId, Vec<TaskId>> = HashMap::new();
    for task in &graph.tasks {
        for &dep in &task.depends_on {
            forward.entry(dep).or_default().push(task.id);
        }
    }

    while let Some(id) = queue.pop_front() {
        if let Some(children) = forward.get(&id) {
            for &child in children {
                if visited.insert(child) {
                    queue.push_back(child);
                }
            }
        }
    }

    visited.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{TaskGraph, TaskId, TaskNode};

    fn make_node(id: u32, deps: &[u32]) -> TaskNode {
        let mut n = TaskNode::new(id, format!("t{id}"), "desc");
        n.depends_on = deps.iter().map(|&d| TaskId(d)).collect();
        n
    }

    fn graph_from(nodes: Vec<TaskNode>) -> TaskGraph {
        let mut g = TaskGraph::new("test");
        g.tasks = nodes;
        g
    }

    fn cfg(threshold: f32) -> CascadeConfig {
        CascadeConfig {
            failure_threshold: threshold,
        }
    }

    // --- ancestor_roots ---

    #[test]
    fn root_task_returns_self() {
        let g = graph_from(vec![make_node(0, &[])]);
        let roots = ancestor_roots(TaskId(0), &g);
        assert_eq!(roots, vec![TaskId(0)]);
    }

    #[test]
    fn linear_chain_root_is_task_zero() {
        // 0 -> 1 -> 2
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[1]),
        ]);
        let roots = ancestor_roots(TaskId(2), &g);
        assert_eq!(roots, vec![TaskId(0)]);
    }

    #[test]
    fn diamond_has_two_roots() {
        // A(0) -> {B(1), C(2)} -> D(3)
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[0]),
            make_node(3, &[1, 2]),
        ]);
        let mut roots = ancestor_roots(TaskId(3), &g);
        roots.sort_by_key(|r| r.as_u32());
        // Only one root (0) because 1 and 2 are not roots themselves.
        assert_eq!(roots, vec![TaskId(0)]);
    }

    #[test]
    fn fan_in_has_multiple_roots() {
        // A(0), B(1), C(2) -> D(3)
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[]),
            make_node(3, &[0, 1, 2]),
        ]);
        let mut roots = ancestor_roots(TaskId(3), &g);
        roots.sort_by_key(|r| r.as_u32());
        assert_eq!(roots, vec![TaskId(0), TaskId(1), TaskId(2)]);
    }

    // --- record_outcome + is_cascading ---

    #[test]
    fn no_failures_not_cascading() {
        let g = graph_from(vec![make_node(0, &[]), make_node(1, &[0])]);
        let mut det = CascadeDetector::new(cfg(0.5));
        det.record_outcome(TaskId(1), true, &g);
        assert!(!det.is_cascading(TaskId(1), &g));
    }

    #[test]
    fn failure_rate_exceeds_threshold() {
        // 0 -> 1, 0 -> 2, 0 -> 3 (fan-out). Two of three fail.
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[0]),
            make_node(3, &[0]),
        ]);
        let mut det = CascadeDetector::new(cfg(0.5));
        det.record_outcome(TaskId(1), false, &g);
        det.record_outcome(TaskId(2), false, &g);
        det.record_outcome(TaskId(3), true, &g);
        // 2 failures / 3 total = 0.67 > 0.5 threshold
        assert!(det.is_cascading(TaskId(1), &g));
        assert!(det.is_cascading(TaskId(2), &g));
        assert!(det.is_cascading(TaskId(3), &g));
    }

    #[test]
    fn reset_clears_all_regions() {
        let g = graph_from(vec![make_node(0, &[]), make_node(1, &[0])]);
        let mut det = CascadeDetector::new(cfg(0.3));
        det.record_outcome(TaskId(1), false, &g);
        det.reset();
        assert!(!det.is_cascading(TaskId(1), &g));
        assert!(det.region_health().is_empty());
    }

    // --- deprioritized_tasks ---

    #[test]
    fn deprioritized_tasks_empty_when_healthy() {
        let g = graph_from(vec![make_node(0, &[]), make_node(1, &[0])]);
        let mut det = CascadeDetector::new(cfg(0.5));
        det.record_outcome(TaskId(1), true, &g);
        assert!(det.deprioritized_tasks(&g).is_empty());
    }

    #[test]
    fn deprioritized_tasks_returns_failing_subtree() {
        // Root 0 -> {1, 2}; Root 3 -> 4. Fail 1 and 2 (region of root 0).
        // Root 3 stays healthy.
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[0]),
            make_node(3, &[]),
            make_node(4, &[3]),
        ]);
        let mut det = CascadeDetector::new(cfg(0.4));
        det.record_outcome(TaskId(1), false, &g);
        det.record_outcome(TaskId(2), false, &g);
        det.record_outcome(TaskId(4), true, &g);
        let dp = det.deprioritized_tasks(&g);
        // Tasks 0, 1, 2 belong to root 0 which is cascading.
        assert!(dp.contains(&TaskId(0)));
        assert!(dp.contains(&TaskId(1)));
        assert!(dp.contains(&TaskId(2)));
        // Tasks 3 and 4 are in healthy region.
        assert!(!dp.contains(&TaskId(3)));
        assert!(!dp.contains(&TaskId(4)));
    }

    #[test]
    fn all_regions_cascading_returns_empty_for_safe_fallback() {
        let g = graph_from(vec![make_node(0, &[]), make_node(1, &[0])]);
        let mut det = CascadeDetector::new(cfg(0.3));
        // Only one region (root 0), make it cascade.
        det.record_outcome(TaskId(1), false, &g);
        // With one region and it cascading, deprioritized_tasks returns empty
        // to prevent complete deadlock (C9 fix).
        let dp = det.deprioritized_tasks(&g);
        assert!(
            dp.is_empty(),
            "all-regions-cascading should return empty to allow forward progress"
        );
    }
}
