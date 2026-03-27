// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Heuristic topology classification for `TaskGraph` DAGs.

use serde::{Deserialize, Serialize};
use zeph_config::OrchestrationConfig;

use super::graph::TaskGraph;

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
    /// None of the above; mixed dependency patterns.
    Mixed,
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
        if edge_count == n - 1 && longest_path(tasks) == n - 1 {
            return Topology::LinearChain;
        }

        // Fan-out: single root, max depth == 1 (root + one layer of leaves only),
        // all non-root tasks depend only on the root.
        let roots_count = tasks.iter().filter(|t| t.depends_on.is_empty()).count();
        if roots_count == 1 && longest_path(tasks) == 1 {
            return Topology::FanOut;
        }

        Topology::Mixed
    }

    /// Suggest a `max_parallel` limit based on topology and config.
    ///
    /// Returns `None` when `topology_selection` is disabled.
    /// The returned value never exceeds `config.max_parallel` and is always >= 1.
    ///
    /// # Performance note
    ///
    /// Parallel overhead only pays off when individual tool calls take >= 500 ms
    /// (typical for LLM API calls). Below that threshold, scheduling cost dominates
    /// and sequential execution may be faster (e.g., local file reads, in-memory lookups).
    #[must_use]
    pub fn suggest_max_parallel(topology: Topology, config: &OrchestrationConfig) -> Option<usize> {
        if !config.topology_selection {
            return None;
        }
        let base = config.max_parallel as usize;
        Some(match topology {
            // No constraints — full configured parallelism.
            Topology::AllParallel | Topology::FanOut => base,
            // Strictly sequential — one at a time.
            Topology::LinearChain => 1,
            // Mixed — conservative: floor(base/2)+1, capped at base, min 1.
            // Fix C1: never exceed base (respects max_parallel=1), never go below 1.
            Topology::Mixed => (base / 2 + 1).min(base).max(1),
        })
    }
}

/// Compute the longest path (in edges) from any root to any leaf using DP on toposort order.
///
/// Assumes a validated DAG (no cycles). Returns 0 for graphs with 0 or 1 tasks.
fn longest_path(tasks: &[super::graph::TaskNode]) -> usize {
    let n = tasks.len();
    if n <= 1 {
        return 0;
    }

    // Kahn's topological sort to get processing order.
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

    max_dist
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
    fn classify_fan_out_with_chain_on_branch_is_mixed() {
        // A(0) -> {B(1), C(2)}, B(1) -> D(3) — depth=2 but only 1 root → Mixed
        let g = graph_from(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[0]),
            make_node(3, &[1]),
        ]);
        // depth=2, roots=1 → not FanOut (depth>1) → Mixed
        assert_eq!(TopologyClassifier::classify(&g), Topology::Mixed);
    }

    // --- suggest_max_parallel tests ---

    #[test]
    fn suggest_disabled_returns_none() {
        let mut cfg = default_config();
        cfg.topology_selection = false;
        assert_eq!(
            TopologyClassifier::suggest_max_parallel(Topology::LinearChain, &cfg),
            None
        );
    }

    #[test]
    fn suggest_linear_chain_returns_one() {
        let cfg = default_config();
        assert_eq!(
            TopologyClassifier::suggest_max_parallel(Topology::LinearChain, &cfg),
            Some(1)
        );
    }

    #[test]
    fn suggest_all_parallel_returns_base() {
        let cfg = default_config(); // max_parallel=4
        assert_eq!(
            TopologyClassifier::suggest_max_parallel(Topology::AllParallel, &cfg),
            Some(4)
        );
    }

    #[test]
    fn suggest_fan_out_returns_base() {
        let cfg = default_config();
        assert_eq!(
            TopologyClassifier::suggest_max_parallel(Topology::FanOut, &cfg),
            Some(4)
        );
    }

    #[test]
    fn suggest_mixed_is_conservative() {
        let cfg = default_config(); // max_parallel=4 → (4/2+1).min(4).max(1) = 3
        assert_eq!(
            TopologyClassifier::suggest_max_parallel(Topology::Mixed, &cfg),
            Some(3)
        );
    }

    #[test]
    fn suggest_mixed_respects_max_parallel_one() {
        // C1 fix: max_parallel=1 must not be overridden to 2
        let mut cfg = default_config();
        cfg.max_parallel = 1;
        // (1/2+1).min(1).max(1) = 1.min(1).max(1) = 1
        assert_eq!(
            TopologyClassifier::suggest_max_parallel(Topology::Mixed, &cfg),
            Some(1)
        );
    }

    #[test]
    fn suggest_mixed_base_two() {
        // base=2 → (2/2+1).min(2).max(1) = 2.min(2).max(1) = 2
        let mut cfg = default_config();
        cfg.max_parallel = 2;
        assert_eq!(
            TopologyClassifier::suggest_max_parallel(Topology::Mixed, &cfg),
            Some(2)
        );
    }
}
