// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Heuristic topology classification for `TaskGraph` DAGs.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use zeph_config::OrchestrationConfig;

use super::graph::{TaskGraph, TaskId, TaskNode};

/// Structural classification of a `TaskGraph`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topology {
    /// All tasks are independent (zero edges). Max parallelism applies.
    AllParallel,
    /// Strict linear chain: each task depends on exactly the previous one.
    LinearChain,
    /// Single root fans out to multiple independent leaves.
    FanOut,
    /// Multiple independent roots converge to a single sink node. Dual of `FanOut`.
    ///
    /// Detection: single node with in-degree >= 2 that is the sole non-root sink,
    /// all other nodes are roots (in-degree 0).
    FanIn,
    /// Multi-level DAG with fan-out at multiple depths (tree-like structure).
    ///
    /// Detection: single root, `longest_path` >= 2, max in-degree == 1 for all non-root nodes.
    Hierarchical,
    /// None of the above; mixed dependency patterns.
    Mixed,
}

/// How the scheduler should dispatch tasks based on topology analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchStrategy {
    /// Dispatch all ready tasks immediately up to `max_parallel`.
    ///
    /// Used for: `AllParallel`, `FanOut`, `FanIn`.
    FullParallel,
    /// Dispatch tasks one at a time in dependency order.
    ///
    /// Used for: `LinearChain`.
    Sequential,
    /// Dispatch tasks level-by-level with a barrier between levels.
    ///
    /// Used for: `Hierarchical`.
    LevelBarrier,
    /// Mix of parallel and sequential based on local subgraph structure.
    ///
    /// Scheduler falls back to default ready-task dispatch with conservative parallelism.
    /// Used for: `Mixed`.
    Adaptive,
}

/// Complete topology analysis result computed in a single O(|V|+|E|) pass.
#[derive(Debug, Clone)]
pub struct TopologyAnalysis {
    pub topology: Topology,
    pub strategy: DispatchStrategy,
    pub max_parallel: usize,
    /// Longest path in the DAG (critical path length).
    pub depth: usize,
    /// Per-task depth from root (BFS level). Used by `LevelBarrier` dispatch.
    ///
    /// Uses `HashMap` so new tasks injected via `inject_tasks()` can be
    /// added without index-out-of-bounds on `Vec` access (critic S3).
    pub depths: HashMap<TaskId, usize>,
}

/// Stateless DAG topology classifier.
pub struct TopologyClassifier;

impl TopologyClassifier {
    /// Classify the topology of a `TaskGraph`.
    ///
    /// Empty graphs return `AllParallel` (no constraints).
    #[must_use]
    pub fn classify(graph: &TaskGraph) -> Topology {
        let tasks = &graph.tasks;
        let n = tasks.len();

        if n == 0 {
            return Topology::AllParallel;
        }

        let edge_count: usize = tasks.iter().map(|t| t.depends_on.len()).sum();

        if edge_count == 0 {
            return Topology::AllParallel;
        }

        // Linear chain: exactly n-1 edges and longest path = n-1
        if edge_count == n - 1 && compute_longest_path_and_depths(tasks).0 == n - 1 {
            return Topology::LinearChain;
        }

        let roots_count = tasks.iter().filter(|t| t.depends_on.is_empty()).count();

        let (longest, _) = compute_longest_path_and_depths(tasks);

        // Fan-out: single root, max depth == 1 (root + one layer of leaves only).
        if roots_count == 1 && longest == 1 {
            return Topology::FanOut;
        }

        // FanIn: multiple roots converge to exactly one sink.
        // The sink has >= 2 dependencies (dep_count >= 2). All other nodes are roots.
        // Depth must be exactly 1.
        let non_roots_count = tasks.iter().filter(|t| !t.depends_on.is_empty()).count();
        if roots_count >= 2 && non_roots_count == 1 && longest == 1 {
            let sink_dep_count = tasks
                .iter()
                .filter(|t| !t.depends_on.is_empty())
                .map(|t| t.depends_on.len())
                .next()
                .unwrap_or(0);
            if sink_dep_count >= 2 {
                return Topology::FanIn;
            }
        }

        // Hierarchical: single root, depth >= 2, max in-degree (dep_count) == 1 for all nodes
        // (tree-like: no node has multiple parents — ensures no diamond patterns).
        if roots_count == 1 && longest >= 2 {
            let max_dep_count = tasks.iter().map(|t| t.depends_on.len()).max().unwrap_or(0);
            if max_dep_count <= 1 {
                return Topology::Hierarchical;
            }
        }

        Topology::Mixed
    }

    /// Map a `Topology` variant to the appropriate `DispatchStrategy`.
    #[must_use]
    pub fn strategy(topology: Topology) -> DispatchStrategy {
        match topology {
            Topology::AllParallel | Topology::FanOut | Topology::FanIn => {
                DispatchStrategy::FullParallel
            }
            Topology::LinearChain => DispatchStrategy::Sequential,
            Topology::Hierarchical => DispatchStrategy::LevelBarrier,
            Topology::Mixed => DispatchStrategy::Adaptive,
        }
    }

    /// Compute a complete `TopologyAnalysis` in a single O(|V|+|E|) pass.
    ///
    /// When `topology_selection` is disabled in config, returns a default
    /// `FullParallel` analysis with config's `max_parallel` — zero overhead.
    ///
    /// # Performance
    ///
    /// Uses a single Kahn's toposort pass to compute both topology classification
    /// and per-task depths simultaneously.
    #[must_use]
    pub fn analyze(graph: &TaskGraph, config: &OrchestrationConfig) -> TopologyAnalysis {
        let tasks = &graph.tasks;
        let n = tasks.len();

        if !config.topology_selection || n == 0 {
            return TopologyAnalysis {
                topology: Topology::AllParallel,
                strategy: DispatchStrategy::FullParallel,
                max_parallel: config.max_parallel as usize,
                depth: 0,
                depths: HashMap::new(),
            };
        }

        let (longest, depths) = compute_longest_path_and_depths(tasks);
        let topology = Self::classify(graph);
        let strategy = Self::strategy(topology);
        let base = config.max_parallel as usize;

        let max_parallel = match topology {
            Topology::AllParallel | Topology::FanOut | Topology::FanIn | Topology::Hierarchical => {
                base
            }
            Topology::LinearChain => 1,
            Topology::Mixed => (base / 2 + 1).min(base).max(1),
        };

        TopologyAnalysis {
            topology,
            strategy,
            max_parallel,
            depth: longest,
            depths,
        }
    }
}

/// Compute depths for the scheduler's dirty re-analysis path.
///
/// Thin wrapper around `compute_longest_path_and_depths` for use by `DagScheduler::tick()`
/// when `topology_dirty=true`.
pub(crate) fn compute_depths_for_scheduler(
    graph: &TaskGraph,
) -> (usize, std::collections::HashMap<TaskId, usize>) {
    compute_longest_path_and_depths(&graph.tasks)
}

/// Compute the longest path and per-task depth map using Kahn's toposort.
///
/// Returns `(longest_path, depths_map)` where `depths_map[task_id] = depth_from_root`.
///
/// Single O(|V|+|E|) pass. Assumes a validated DAG (no cycles).
fn compute_longest_path_and_depths(tasks: &[TaskNode]) -> (usize, HashMap<TaskId, usize>) {
    let n = tasks.len();
    if n == 0 {
        return (0, HashMap::new());
    }

    let mut in_degree = vec![0usize; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
    for task in tasks {
        let i = task.id.index();
        in_degree[i] = task.depends_on.len();
        for dep in &task.depends_on {
            dependents[dep.index()].push(i);
        }
    }

    let mut queue: std::collections::VecDeque<usize> = in_degree
        .iter()
        .enumerate()
        .filter(|(_, d)| **d == 0)
        .map(|(i, _)| i)
        .collect();

    let mut dist = vec![0usize; n];
    let mut max_dist = 0usize;

    while let Some(u) = queue.pop_front() {
        for &v in &dependents[u] {
            let new_dist = dist[u] + 1;
            if new_dist > dist[v] {
                dist[v] = new_dist;
            }
            if dist[v] > max_dist {
                max_dist = dist[v];
            }
            in_degree[v] -= 1;
            if in_degree[v] == 0 {
                queue.push_back(v);
            }
        }
    }

    let depths: HashMap<TaskId, usize> = tasks.iter().map(|t| (t.id, dist[t.id.index()])).collect();

    (max_dist, depths)
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

    fn default_config() -> zeph_config::OrchestrationConfig {
        zeph_config::OrchestrationConfig {
            topology_selection: true,
            max_parallel: 4,
            ..zeph_config::OrchestrationConfig::default()
        }
    }

    // --- classify tests ---

    #[test]
    fn classify_empty_graph() {
        let g = graph_from(vec![]);
        assert_eq!(TopologyClassifier::classify(&g), Topology::AllParallel);
    }

    #[test]
    fn classify_single_task() {
        let g = graph_from(vec![make_node(0, &[])]);
        assert_eq!(TopologyClassifier::classify(&g), Topology::AllParallel);
    }

    #[test]
    fn classify_all_parallel() {
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[]),
        ]);
        assert_eq!(TopologyClassifier::classify(&g), Topology::AllParallel);
    }

    #[test]
    fn classify_two_task_chain() {
        // A(0) -> B(1)
        let g = graph_from(vec![make_node(0, &[]), make_node(1, &[0])]);
        assert_eq!(TopologyClassifier::classify(&g), Topology::LinearChain);
    }

    #[test]
    fn classify_linear_chain() {
        // A(0) -> B(1) -> C(2)
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[1]),
        ]);
        assert_eq!(TopologyClassifier::classify(&g), Topology::LinearChain);
    }

    #[test]
    fn classify_fan_out() {
        // A(0) -> {B(1), C(2), D(3)}
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[0]),
            make_node(3, &[0]),
        ]);
        assert_eq!(TopologyClassifier::classify(&g), Topology::FanOut);
    }

    #[test]
    fn classify_fan_in() {
        // A(0), B(1), C(2) all -> D(3): multiple roots, single sink
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[]),
            make_node(3, &[0, 1, 2]),
        ]);
        assert_eq!(TopologyClassifier::classify(&g), Topology::FanIn);
    }

    #[test]
    fn classify_fan_in_two_roots() {
        // A(0), B(1) -> C(2)
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[0, 1]),
        ]);
        assert_eq!(TopologyClassifier::classify(&g), Topology::FanIn);
    }

    #[test]
    fn classify_hierarchical() {
        // A(0) -> {B(1), C(2)}, B(1) -> D(3), C(2) -> E(4)
        // Single root, depth=2, max in-degree=1 for non-roots
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[0]),
            make_node(3, &[1]),
            make_node(4, &[2]),
        ]);
        assert_eq!(TopologyClassifier::classify(&g), Topology::Hierarchical);
    }

    #[test]
    fn classify_hierarchical_three_levels() {
        // A(0) -> B(1) -> C(2) -> D(3): linear but single root, depth=3, in-degree<=1 => Hierarchical?
        // No — linear chain is caught first (n-1 edges, longest=n-1). This tests a tree.
        // A(0) -> {B(1), C(2)}, B(1) -> D(3)
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[0]),
            make_node(3, &[1]),
        ]);
        assert_eq!(TopologyClassifier::classify(&g), Topology::Hierarchical);
    }

    #[test]
    fn classify_diamond_is_mixed() {
        // A(0) -> {B(1), C(2)} -> D(3)
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[0]),
            make_node(3, &[1, 2]),
        ]);
        assert_eq!(TopologyClassifier::classify(&g), Topology::Mixed);
    }

    #[test]
    fn classify_fan_out_with_chain_on_branch_is_hierarchical() {
        // A(0) -> {B(1), C(2)}, B(1) -> D(3) — single root, depth=2, all in-degrees <= 1 → Hierarchical
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[0]),
            make_node(3, &[1]),
        ]);
        assert_eq!(TopologyClassifier::classify(&g), Topology::Hierarchical);
    }

    // --- strategy tests ---

    #[test]
    fn strategy_all_parallel_is_full_parallel() {
        assert_eq!(
            TopologyClassifier::strategy(Topology::AllParallel),
            DispatchStrategy::FullParallel
        );
    }

    #[test]
    fn strategy_fan_out_is_full_parallel() {
        assert_eq!(
            TopologyClassifier::strategy(Topology::FanOut),
            DispatchStrategy::FullParallel
        );
    }

    #[test]
    fn strategy_fan_in_is_full_parallel() {
        assert_eq!(
            TopologyClassifier::strategy(Topology::FanIn),
            DispatchStrategy::FullParallel
        );
    }

    #[test]
    fn strategy_linear_chain_is_sequential() {
        assert_eq!(
            TopologyClassifier::strategy(Topology::LinearChain),
            DispatchStrategy::Sequential
        );
    }

    #[test]
    fn strategy_hierarchical_is_level_barrier() {
        assert_eq!(
            TopologyClassifier::strategy(Topology::Hierarchical),
            DispatchStrategy::LevelBarrier
        );
    }

    #[test]
    fn strategy_mixed_is_adaptive() {
        assert_eq!(
            TopologyClassifier::strategy(Topology::Mixed),
            DispatchStrategy::Adaptive
        );
    }

    // --- analyze tests ---

    #[test]
    fn analyze_disabled_returns_full_parallel() {
        let mut cfg = default_config();
        cfg.topology_selection = false;
        let g = graph_from(vec![make_node(0, &[]), make_node(1, &[0])]);
        let analysis = TopologyClassifier::analyze(&g, &cfg);
        assert_eq!(analysis.strategy, DispatchStrategy::FullParallel);
        assert_eq!(analysis.max_parallel, 4);
        assert_eq!(analysis.topology, Topology::AllParallel);
    }

    #[test]
    fn analyze_linear_chain_returns_sequential() {
        let cfg = default_config();
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[1]),
        ]);
        let analysis = TopologyClassifier::analyze(&g, &cfg);
        assert_eq!(analysis.topology, Topology::LinearChain);
        assert_eq!(analysis.strategy, DispatchStrategy::Sequential);
        assert_eq!(analysis.max_parallel, 1);
        assert_eq!(analysis.depth, 2);
    }

    #[test]
    fn analyze_hierarchical_returns_level_barrier() {
        let cfg = default_config();
        // A(0) -> {B(1), C(2)}, B(1) -> D(3)
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[0]),
            make_node(3, &[1]),
        ]);
        let analysis = TopologyClassifier::analyze(&g, &cfg);
        assert_eq!(analysis.topology, Topology::Hierarchical);
        assert_eq!(analysis.strategy, DispatchStrategy::LevelBarrier);
        assert_eq!(analysis.max_parallel, 4);
        assert_eq!(analysis.depth, 2);
        // Verify depths
        assert_eq!(analysis.depths[&TaskId(0)], 0);
        assert_eq!(analysis.depths[&TaskId(1)], 1);
        assert_eq!(analysis.depths[&TaskId(2)], 1);
        assert_eq!(analysis.depths[&TaskId(3)], 2);
    }

    #[test]
    fn analyze_fan_in_returns_full_parallel() {
        let cfg = default_config();
        // A(0), B(1), C(2) -> D(3)
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[]),
            make_node(3, &[0, 1, 2]),
        ]);
        let analysis = TopologyClassifier::analyze(&g, &cfg);
        assert_eq!(analysis.topology, Topology::FanIn);
        assert_eq!(analysis.strategy, DispatchStrategy::FullParallel);
        assert_eq!(analysis.max_parallel, 4);
    }

    #[test]
    fn analyze_mixed_is_conservative() {
        let cfg = default_config(); // max_parallel=4 -> (4/2+1).min(4).max(1) = 3
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[0]),
            make_node(3, &[1, 2]),
        ]);
        let analysis = TopologyClassifier::analyze(&g, &cfg);
        assert_eq!(analysis.topology, Topology::Mixed);
        assert_eq!(analysis.strategy, DispatchStrategy::Adaptive);
        assert_eq!(analysis.max_parallel, 3);
    }

    #[test]
    fn analyze_depths_correct_for_fan_out() {
        let cfg = default_config();
        // A(0) -> {B(1), C(2), D(3)}
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[0]),
            make_node(3, &[0]),
        ]);
        let analysis = TopologyClassifier::analyze(&g, &cfg);
        assert_eq!(analysis.depths[&TaskId(0)], 0);
        assert_eq!(analysis.depths[&TaskId(1)], 1);
        assert_eq!(analysis.depths[&TaskId(2)], 1);
        assert_eq!(analysis.depths[&TaskId(3)], 1);
    }

    #[test]
    fn analyze_mixed_respects_max_parallel_one() {
        let mut cfg = default_config();
        cfg.max_parallel = 1;
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[0]),
            make_node(3, &[1, 2]),
        ]);
        let analysis = TopologyClassifier::analyze(&g, &cfg);
        assert_eq!(analysis.max_parallel, 1);
    }
}
