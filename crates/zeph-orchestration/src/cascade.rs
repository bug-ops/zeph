// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Cascade-aware routing for DAG execution (arXiv:2603.17112).
//!
//! Tracks failure propagation across DAG regions. When a subtree's failure rate
//! exceeds the configured threshold, tasks in that subtree are deprioritised in
//! [`DagScheduler::tick`] so that healthy independent branches run first.
//!
//! A "region" is defined by the "heaviest root" of each task — the root ancestor
//! that reaches the most downstream tasks. This prevents over-aggressive deprioritisation
//! on diamond-shaped DAGs.
//!
//! [`DagScheduler::tick`]: crate::scheduler::DagScheduler::tick

use std::collections::{HashSet, VecDeque};

use super::graph::{TaskGraph, TaskId};

/// Decision returned by [`CascadeDetector::evaluate_abort`].
///
/// Callers match on this to decide whether to abort the DAG immediately.
///
/// # Examples
///
/// ```rust
/// use zeph_orchestration::cascade::AbortDecision;
/// use zeph_orchestration::graph::TaskId;
///
/// let decision = AbortDecision::None;
/// assert!(matches!(decision, AbortDecision::None));
/// ```
#[derive(Debug, Clone)]
pub enum AbortDecision {
    /// No abort warranted; continue normal execution.
    None,
    /// A DAG region's failure rate exceeded the configured threshold.
    FanOutCascade {
        /// The root task ID of the failing region.
        region_root: TaskId,
        /// Failure rate at the time of abort (0.0–1.0).
        failure_rate: f32,
        /// Total tasks observed in the region (completed + failed).
        region_size: usize,
    },
}

/// Per-region failure health snapshot.
///
/// Accumulated by [`CascadeDetector::record_outcome`] and read by
/// [`CascadeDetector::is_cascading`] and [`CascadeDetector::deprioritized_tasks`].
#[derive(Debug, Clone)]
pub struct RegionHealth {
    /// Total number of task outcomes recorded for this region.
    pub total_tasks: usize,
    /// Number of failed task outcomes.
    pub failed_tasks: usize,
    /// `failed_tasks / total_tasks`. `0.0` when `total_tasks = 0`.
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

/// Configuration for cascade detection.
///
/// Extracted from `OrchestrationConfig` at [`DagScheduler`] construction time.
///
/// [`DagScheduler`]: crate::scheduler::DagScheduler
#[derive(Debug, Clone)]
pub struct CascadeConfig {
    /// Failure rate (0.0–1.0) above which a region is considered "cascading".
    ///
    /// For example, `0.5` means more than half of the completed tasks in a
    /// region must have failed before it is deprioritised.
    pub failure_threshold: f32,
}

/// Tracks failure propagation across DAG regions for cascade-aware routing.
///
/// A "region" is the set of tasks reachable from the "heaviest root" of each task — the
/// root ancestor that is the source of the most downstream tasks. For tasks with multiple
/// roots (diamond patterns), we pick the single root that covers the most descendants,
/// preventing over-aggressive deprioritisation on diamond-shaped DAGs.
///
/// ## Caching
///
/// The forward adjacency map (task → direct dependents) is computed lazily on first use
/// and reused until `reset()` is called. Cache invariants:
///
/// - `forward_adjacency.is_none()` OR
///   `forward_adjacency.as_ref().unwrap().len() == graph.tasks.len()` at use time.
/// - Indexed by `TaskId::index()`; relies on the dense-id invariant in [`TaskGraph`].
/// - Any post-construction mutation of `graph.tasks` (push, truncate, reorder) OR of any
///   `TaskNode::depends_on` on a task already in `graph.tasks` MUST be followed by
///   [`CascadeDetector::reset`]. `inject_tasks` and its callers currently guarantee this;
///   no other mutation path exists in-tree.
#[derive(Debug)]
pub struct CascadeDetector {
    config: CascadeConfig,
    /// Per-root failure health. Key = root `TaskId`.
    region_health: std::collections::HashMap<TaskId, RegionHealth>,
    /// Cached forward adjacency: `forward_adjacency[i]` is the list of task IDs that
    /// depend on `TaskId(i)`. Populated lazily on first use; invalidated by `reset()`.
    ///
    /// `None` means not yet built or just invalidated. See struct-level "Caching" section.
    forward_adjacency: Option<Vec<Vec<TaskId>>>,
}

impl CascadeDetector {
    /// Create a new detector.
    #[must_use]
    pub fn new(config: CascadeConfig) -> Self {
        Self {
            config,
            region_health: std::collections::HashMap::new(),
            forward_adjacency: None,
        }
    }

    /// Record a task outcome and update the health of its primary region.
    pub fn record_outcome(&mut self, task_id: TaskId, succeeded: bool, graph: &TaskGraph) {
        let root = self.primary_root(task_id, graph);
        self.region_health
            .entry(root)
            .or_insert_with(RegionHealth::new)
            .record(!succeeded);
    }

    /// Returns `true` when the primary region of `task_id` is in cascade failure.
    #[must_use]
    pub fn is_cascading(&mut self, task_id: TaskId, graph: &TaskGraph) -> bool {
        let root = self.primary_root(task_id, graph);
        self.region_health
            .get(&root)
            .is_some_and(|h| h.failure_rate > self.config.failure_threshold)
    }

    /// Returns the set of task IDs that should be deprioritized due to cascade failure.
    ///
    /// Returns an empty set when no region is cascading, avoiding unnecessary reordering.
    #[must_use]
    pub fn deprioritized_tasks(&mut self, graph: &TaskGraph) -> HashSet<TaskId> {
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

        // Collect task ids first to avoid borrow conflict with &mut self in primary_root.
        let task_ids: Vec<TaskId> = graph.tasks.iter().map(|t| t.id).collect();
        task_ids
            .into_iter()
            .filter(|&id| cascading_roots.contains(&self.primary_root(id, graph)))
            .collect()
    }

    /// Evaluate whether a cascade abort should be triggered by fan-out failure rate.
    ///
    /// Returns [`AbortDecision::FanOutCascade`] when the primary region of `failed_task_id`
    /// has a failure rate ≥ `rate_threshold` AND the region has at least 3 tasks (floor
    /// prevents a single-failure 100%-rate region from triggering an abort prematurely).
    ///
    /// Returns [`AbortDecision::None`] when `rate_threshold ≤ 0.0` (disabled by default).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_orchestration::cascade::{AbortDecision, CascadeConfig, CascadeDetector};
    /// use zeph_orchestration::graph::{TaskGraph, TaskId, TaskNode};
    ///
    /// fn make_node(id: u32, deps: &[u32]) -> TaskNode {
    ///     let mut n = TaskNode::new(id, format!("t{id}"), "desc");
    ///     n.depends_on = deps.iter().map(|&d| TaskId(d)).collect();
    ///     n
    /// }
    ///
    /// let mut g = TaskGraph::new("test");
    /// g.tasks = vec![
    ///     make_node(0, &[]),
    ///     make_node(1, &[0]),
    ///     make_node(2, &[0]),
    ///     make_node(3, &[0]),
    /// ];
    ///
    /// let mut det = CascadeDetector::new(CascadeConfig { failure_threshold: 0.5 });
    /// det.record_outcome(TaskId(1), false, &g);
    /// det.record_outcome(TaskId(2), false, &g);
    /// det.record_outcome(TaskId(3), true, &g);
    ///
    /// // 2/3 failures = 0.67 >= threshold 0.7? No, so None here.
    /// match det.evaluate_abort(&g, TaskId(1), 0.9) {
    ///     AbortDecision::None => {}
    ///     other => panic!("unexpected: {:?}", other),
    /// }
    /// ```
    #[must_use]
    pub fn evaluate_abort(
        &mut self,
        graph: &TaskGraph,
        failed_task_id: TaskId,
        rate_threshold: f32,
    ) -> AbortDecision {
        if rate_threshold <= f32::EPSILON {
            return AbortDecision::None;
        }
        let root = self.primary_root(failed_task_id, graph);
        if let Some(health) = self.region_health.get(&root)
            && health.failure_rate >= rate_threshold
            && health.total_tasks >= 3
        {
            return AbortDecision::FanOutCascade {
                region_root: root,
                failure_rate: health.failure_rate,
                region_size: health.total_tasks,
            };
        }
        AbortDecision::None
    }

    /// Reset all region health counters and invalidate the cached forward adjacency.
    ///
    /// Called by `DagScheduler::inject_tasks()` because graph topology has fundamentally
    /// changed — old failure counts no longer reflect the new task set, and the cached
    /// forward-adjacency map no longer matches the new `graph.tasks`.
    pub fn reset(&mut self) {
        self.region_health.clear();
        self.forward_adjacency = None;
    }

    /// Expose region health for testing.
    #[cfg(test)]
    #[must_use]
    pub fn region_health(&self) -> &std::collections::HashMap<TaskId, RegionHealth> {
        &self.region_health
    }

    /// Returns `true` when the forward adjacency cache is populated.
    #[cfg(test)]
    #[must_use]
    pub fn forward_adjacency_is_cached(&self) -> bool {
        self.forward_adjacency.is_some()
    }

    /// Build the forward adjacency cache on first use; return the slice on subsequent calls.
    ///
    /// Asserts (in debug builds) that a cached vector has the same length as `graph.tasks`,
    /// catching stale-cache bugs where `graph.tasks` was mutated without calling `reset()`.
    fn ensure_adjacency(&mut self, graph: &TaskGraph) -> &[Vec<TaskId>] {
        if self.forward_adjacency.is_none() {
            let mut forward: Vec<Vec<TaskId>> = vec![Vec::new(); graph.tasks.len()];
            for task in &graph.tasks {
                for &dep in &task.depends_on {
                    forward[dep.index()].push(task.id);
                }
            }
            self.forward_adjacency = Some(forward);
        } else if let Some(ref fwd) = self.forward_adjacency {
            debug_assert_eq!(
                fwd.len(),
                graph.tasks.len(),
                "forward_adjacency stale: graph.tasks was mutated without CascadeDetector::reset()"
            );
        }
        self.forward_adjacency.as_deref().unwrap()
    }

    /// Compute the "heaviest" root for `task_id` using the cached forward adjacency.
    fn primary_root(&mut self, task_id: TaskId, graph: &TaskGraph) -> TaskId {
        let roots = ancestor_roots(task_id, graph);
        if roots.is_empty() {
            return task_id;
        }
        if roots.len() == 1 {
            return roots[0];
        }
        roots
            .into_iter()
            .max_by_key(|&r| (self.descendant_count(r, graph), u32::MAX - r.as_u32()))
            .unwrap_or(task_id)
    }

    /// Count tasks reachable from `root` (inclusive) using the cached forward adjacency.
    fn descendant_count(&mut self, root: TaskId, graph: &TaskGraph) -> usize {
        // Ensure the cache is populated, then take ownership of the slice length
        // by pre-collecting children into a local work-list to avoid a long-lived
        // borrow of `self` that would conflict with the mutable `ensure_adjacency`.
        self.ensure_adjacency(graph);
        let fwd = self.forward_adjacency.as_ref().unwrap();

        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(root);
        visited.insert(root);
        while let Some(id) = queue.pop_front() {
            if let Some(children) = fwd.get(id.index()) {
                for &child in children {
                    if visited.insert(child) {
                        queue.push_back(child);
                    }
                }
            }
        }
        visited.len()
    }
}

/// Collect all root (in-degree 0) ancestors of `task_id` via BFS.
fn ancestor_roots(task_id: TaskId, graph: &TaskGraph) -> Vec<TaskId> {
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
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
        // to prevent complete deadlock when all regions fail simultaneously.
        let dp = det.deprioritized_tasks(&g);
        assert!(
            dp.is_empty(),
            "all-regions-cascading should return empty to allow forward progress"
        );
    }

    // --- forward adjacency cache ---

    #[test]
    fn cache_populated_after_first_call_multi_root() {
        // Fan-in: A(0), B(1) -> C(2). Task C has two roots, so descendant_count is called
        // and the adjacency cache is built.
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[0, 1]),
        ]);
        let mut det = CascadeDetector::new(cfg(0.5));
        assert!(!det.forward_adjacency_is_cached());
        det.record_outcome(TaskId(2), true, &g);
        assert!(det.forward_adjacency_is_cached());
    }

    #[test]
    fn cache_still_populated_on_second_call() {
        // Fan-in: same multi-root graph to ensure cache is built on first call.
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[0, 1]),
        ]);
        let mut det = CascadeDetector::new(cfg(0.5));
        det.record_outcome(TaskId(2), true, &g);
        assert!(det.forward_adjacency_is_cached());
        // Second call must not rebuild (cache stays populated).
        det.record_outcome(TaskId(2), false, &g);
        assert!(det.forward_adjacency_is_cached());
    }

    #[test]
    fn reset_clears_forward_adjacency() {
        // Use multi-root graph so the cache is populated on record_outcome.
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[0, 1]),
        ]);
        let mut det = CascadeDetector::new(cfg(0.3));
        det.record_outcome(TaskId(2), false, &g);
        assert!(det.forward_adjacency_is_cached());
        det.reset();
        assert!(!det.forward_adjacency_is_cached());
    }

    #[test]
    fn primary_root_consistent_with_and_without_cache() {
        // Fan-in: two independent roots A(0) and B(1) both feed into C(2).
        // C has two root ancestors, so descendant_count is called and cache is built.
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[0, 1]),
        ]);
        let mut det = CascadeDetector::new(cfg(0.5));
        // First call builds cache.
        let root_cold = det.primary_root(TaskId(2), &g);
        assert!(det.forward_adjacency_is_cached());
        // Second call uses cache; result must be identical.
        let root_warm = det.primary_root(TaskId(2), &g);
        assert_eq!(root_cold, root_warm);
    }
}
