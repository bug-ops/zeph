// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Scheduling decision logic: topology re-analysis, level barrier advancement,
//! and graph completion detection.

use super::{DagScheduler, SchedulerAction};
use crate::dag;
use crate::graph::{GraphStatus, TaskStatus};
use crate::topology::{DispatchStrategy, Topology, TopologyAnalysis, TopologyClassifier};

impl DagScheduler {
    /// Re-analyze topology when marked dirty by `inject_tasks`.
    pub(super) fn reanalyze_topology_if_dirty(&mut self) {
        if !self.topology_dirty {
            return;
        }
        let new_analysis = {
            let n = self.graph.tasks.len();
            if n == 0 {
                TopologyAnalysis {
                    topology: Topology::AllParallel,
                    strategy: DispatchStrategy::FullParallel,
                    max_parallel: self.config_max_parallel,
                    depth: 0,
                    depths: std::collections::HashMap::new(),
                }
            } else {
                let (depth, depths) = crate::topology::compute_depths_for_scheduler(&self.graph);
                let topo = TopologyClassifier::classify_with_depths(&self.graph, depth, &depths);
                let strategy_config = zeph_config::OrchestrationConfig {
                    cascade_routing: self.cascade_routing,
                    tree_optimized_dispatch: self.tree_optimized_dispatch,
                    ..zeph_config::OrchestrationConfig::default()
                };
                let strategy = TopologyClassifier::strategy(topo, &strategy_config);
                let max_parallel =
                    TopologyClassifier::compute_max_parallel(topo, self.config_max_parallel);
                TopologyAnalysis {
                    topology: topo,
                    strategy,
                    max_parallel,
                    depth,
                    depths,
                }
            }
        };
        self.topology = new_analysis;
        self.max_parallel = self.topology.max_parallel;
        self.topology_dirty = false;
        if self.topology.strategy == DispatchStrategy::LevelBarrier {
            let min_active = self
                .graph
                .tasks
                .iter()
                .filter(|t| !t.status.is_terminal())
                .filter_map(|t| self.topology.depths.get(&t.id).copied())
                .min();
            if let Some(min_depth) = min_active {
                self.current_level = self.current_level.min(min_depth);
            }
        }
    }

    /// Advance the `LevelBarrier` level when all tasks at the current level are terminal.
    pub(super) fn advance_level_barrier_if_needed(&mut self) {
        if self.topology.strategy != DispatchStrategy::LevelBarrier {
            return;
        }
        let all_current_level_terminal = self.graph.tasks.iter().all(|t| {
            let task_depth = self
                .topology
                .depths
                .get(&t.id)
                .copied()
                .unwrap_or(usize::MAX);
            task_depth != self.current_level || t.status.is_terminal()
        });
        if all_current_level_terminal {
            let max_depth = self.topology.depth;
            while self.current_level <= max_depth {
                let has_non_terminal = self.graph.tasks.iter().any(|t| {
                    let d = self
                        .topology
                        .depths
                        .get(&t.id)
                        .copied()
                        .unwrap_or(usize::MAX);
                    d == self.current_level && !t.status.is_terminal()
                });
                if has_non_terminal {
                    break;
                }
                self.current_level += 1;
            }
        }
    }

    /// Emit `Done` if the graph has reached a terminal state or detect deadlock.
    pub(super) fn check_graph_completion(&mut self) -> Vec<SchedulerAction> {
        let running_in_graph_now = self
            .graph
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Running)
            .count();
        if running_in_graph_now != 0 || !self.running.is_empty() {
            return vec![];
        }
        let all_terminal = self.graph.tasks.iter().all(|t| t.status.is_terminal());
        if all_terminal {
            self.graph.status = GraphStatus::Completed;
            self.graph.finished_at = Some(crate::graph::chrono_now());
            return vec![SchedulerAction::Done {
                status: GraphStatus::Completed,
            }];
        }
        // Not a deadlock when predicate evaluation is pending — the scheduler is waiting
        // for record_predicate_outcome() to be called from the agent loop.
        let predicate_pending = self.verify_predicate_enabled
            && self.graph.tasks.iter().any(|t| {
                t.status == TaskStatus::Completed
                    && t.verify_predicate.is_some()
                    && t.predicate_outcome.is_none()
            });
        if predicate_pending {
            return vec![];
        }

        if dag::ready_tasks(&self.graph).is_empty() {
            tracing::error!(
                "scheduler deadlock: no running or ready tasks, but graph not complete"
            );
            self.graph.status = GraphStatus::Failed;
            self.graph.finished_at = Some(crate::graph::chrono_now());
            debug_assert!(
                self.running.is_empty(),
                "deadlock branch reached with non-empty running map"
            );
            for task in &mut self.graph.tasks {
                if !task.status.is_terminal() {
                    task.status = TaskStatus::Canceled;
                }
            }
            return vec![SchedulerAction::Done {
                status: GraphStatus::Failed,
            }];
        }
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use crate::graph::TaskStatus;
    use crate::scheduler::DagScheduler;
    use crate::scheduler::SchedulerAction;
    use crate::scheduler::tests::*;

    // --- topology_selection tests ---

    #[test]
    fn topology_linear_chain_limits_parallelism_to_one() {
        let graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[1]),
        ]);
        let config = zeph_config::OrchestrationConfig {
            topology_selection: true,
            max_parallel: 4,
            ..make_config()
        };
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
            None,
        )
        .unwrap();

        assert_eq!(
            scheduler.topology().topology,
            crate::topology::Topology::LinearChain
        );
        assert_eq!(scheduler.max_parallel, 1);

        let actions = scheduler.tick();
        let spawn_count = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
            .count();
        assert_eq!(spawn_count, 1, "linear chain: only 1 task dispatched");
    }

    #[test]
    fn topology_all_parallel_dispatches_all_ready() {
        let graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[]),
            make_node(3, &[]),
        ]);
        let config = zeph_config::OrchestrationConfig {
            topology_selection: true,
            max_parallel: 4,
            ..make_config()
        };
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
            None,
        )
        .unwrap();

        assert_eq!(
            scheduler.topology().topology,
            crate::topology::Topology::AllParallel
        );

        let actions = scheduler.tick();
        let spawn_count = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
            .count();
        assert_eq!(spawn_count, 4, "all-parallel: all 4 tasks dispatched");
    }

    #[test]
    fn sequential_dispatch_one_at_a_time_parallel_unblocked() {
        use crate::graph::{ExecutionMode, TaskId};

        let mut a = make_node(0, &[]);
        a.execution_mode = ExecutionMode::Sequential;
        let mut b = make_node(1, &[]);
        b.execution_mode = ExecutionMode::Sequential;
        let mut c = make_node(2, &[]);
        c.execution_mode = ExecutionMode::Parallel;

        let graph = graph_from_nodes(vec![a, b, c]);
        let config = zeph_config::OrchestrationConfig {
            max_parallel: 4,
            ..make_config()
        };
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
            None,
        )
        .unwrap();

        let actions = scheduler.tick();
        let spawned: Vec<TaskId> = actions
            .iter()
            .filter_map(|a| {
                if let SchedulerAction::Spawn { task_id, .. } = a {
                    Some(*task_id)
                } else {
                    None
                }
            })
            .collect();

        assert!(
            spawned.contains(&TaskId(0)),
            "A(sequential) must be dispatched"
        );
        assert!(
            spawned.contains(&TaskId(2)),
            "C(parallel) must be dispatched"
        );
        assert!(!spawned.contains(&TaskId(1)), "B(sequential) must be held");
        assert_eq!(spawned.len(), 2);
    }

    // --- LevelBarrier dispatch tests ---

    fn make_hierarchical_config() -> zeph_config::OrchestrationConfig {
        zeph_config::OrchestrationConfig {
            topology_selection: true,
            max_parallel: 4,
            ..make_config()
        }
    }

    /// A(0)→{B(1),C(2)}, B(1)→D(3). Hierarchical topology.
    fn make_hierarchical_graph() -> crate::graph::TaskGraph {
        graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[0]),
            make_node(3, &[1]),
        ])
    }

    #[test]
    fn test_level_barrier_advances_on_terminal_level() {
        use crate::graph::TaskId;

        let graph = make_hierarchical_graph();
        let config = make_hierarchical_config();
        let defs = vec![make_def("worker")];
        let mut scheduler =
            DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();

        assert_eq!(
            scheduler.topology().strategy,
            crate::topology::DispatchStrategy::LevelBarrier,
        );
        assert_eq!(scheduler.current_level, 0);

        let actions = scheduler.tick();
        let spawned_ids: Vec<_> = actions
            .iter()
            .filter_map(|a| {
                if let SchedulerAction::Spawn { task_id, .. } = a {
                    Some(*task_id)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(spawned_ids, vec![TaskId(0)]);

        scheduler.graph.tasks[0].status = TaskStatus::Completed;
        scheduler.running.clear();
        scheduler.graph.tasks[1].status = TaskStatus::Ready;
        scheduler.graph.tasks[2].status = TaskStatus::Ready;

        let actions2 = scheduler.tick();
        assert_eq!(scheduler.current_level, 1);
        let spawned2: Vec<_> = actions2
            .iter()
            .filter_map(|a| {
                if let SchedulerAction::Spawn { task_id, .. } = a {
                    Some(*task_id)
                } else {
                    None
                }
            })
            .collect();
        assert!(spawned2.contains(&TaskId(1)));
        assert!(spawned2.contains(&TaskId(2)));
    }

    #[test]
    fn test_level_barrier_failure_propagates_transitively() {
        use crate::graph::TaskId;
        use crate::scheduler::{RunningTask, TaskEvent, TaskOutcome};

        let graph = make_hierarchical_graph();
        let config = make_hierarchical_config();
        let defs = vec![make_def("worker")];
        let mut scheduler =
            DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();

        scheduler.graph.tasks[0].failure_strategy = Some(crate::graph::FailureStrategy::Skip);
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "h0".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: std::time::Instant::now(),
                admission_permit: None,
            },
        );

        scheduler.buffered_events.push_back(TaskEvent {
            task_id: TaskId(0),
            agent_handle_id: "h0".to_string(),
            outcome: TaskOutcome::Failed {
                error: "simulated failure".to_string(),
            },
        });

        scheduler.tick();

        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Skipped);
        assert_eq!(scheduler.graph.tasks[1].status, TaskStatus::Skipped);
        assert_eq!(scheduler.graph.tasks[2].status, TaskStatus::Skipped);
        assert_eq!(scheduler.graph.tasks[3].status, TaskStatus::Skipped);
    }

    #[test]
    fn test_level_barrier_current_level_reset_after_inject() {
        use crate::graph::TaskId;

        let graph = make_hierarchical_graph();
        let config = make_hierarchical_config();
        let defs = vec![make_def("worker")];
        let mut scheduler =
            DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();

        scheduler.graph.tasks[0].status = TaskStatus::Completed;
        scheduler.graph.tasks[1].status = TaskStatus::Completed;
        scheduler.graph.tasks[2].status = TaskStatus::Completed;
        scheduler.current_level = 2;

        let e = make_node(4, &[0]);
        scheduler.inject_tasks(TaskId(3), vec![e], 20).unwrap();
        assert!(scheduler.topology_dirty);

        scheduler.tick();
        assert_eq!(scheduler.current_level, 1);
    }

    #[test]
    fn resume_from_preserves_topology_classification() {
        use crate::graph::GraphStatus;

        let mut graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[1]),
        ]);
        graph.status = GraphStatus::Paused;
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[1].status = TaskStatus::Pending;
        graph.tasks[2].status = TaskStatus::Pending;

        let config = zeph_config::OrchestrationConfig {
            topology_selection: true,
            max_parallel: 4,
            ..make_config()
        };
        let scheduler = DagScheduler::resume_from(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
            None,
        )
        .unwrap();

        assert_eq!(
            scheduler.topology().topology,
            crate::topology::Topology::LinearChain,
        );
        assert_eq!(scheduler.max_parallel, 1);
    }

    #[test]
    fn config_max_parallel_initialized_from_config() {
        let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        let config = zeph_config::OrchestrationConfig {
            topology_selection: true,
            max_parallel: 6,
            ..make_config()
        };
        let scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
            None,
        )
        .unwrap();

        assert_eq!(scheduler.config_max_parallel, 6);
        assert_eq!(scheduler.max_parallel, 1);
    }

    #[test]
    fn max_parallel_does_not_drift_across_inject_tick_cycles() {
        use crate::graph::TaskId;

        let graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[0]),
            make_node(3, &[1, 2]),
        ]);
        let config = zeph_config::OrchestrationConfig {
            topology_selection: true,
            max_parallel: 4,
            max_tasks: 50,
            ..make_config()
        };
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
            None,
        )
        .unwrap();

        assert_eq!(
            scheduler.topology().topology,
            crate::topology::Topology::Mixed
        );
        let expected_max_parallel = (4usize / 2 + 1).clamp(1, 4);
        assert_eq!(scheduler.max_parallel, expected_max_parallel);

        let extra_task_id = 4u32;
        let extra_task = {
            let mut n = crate::graph::TaskNode::new(
                extra_task_id,
                "extra".to_string(),
                "extra task injected by replan",
            );
            n.depends_on = vec![TaskId(3)];
            n
        };

        scheduler.graph.tasks[3].status = TaskStatus::Completed;
        scheduler
            .inject_tasks(TaskId(3), vec![extra_task], 50)
            .expect("inject must succeed");
        assert!(scheduler.topology_dirty);

        let _ = scheduler.tick();
        assert_eq!(
            scheduler.max_parallel, expected_max_parallel,
            "max_parallel must not drift after first inject+tick"
        );

        let extra_task2 = {
            let mut n = crate::graph::TaskNode::new(5u32, "extra2".to_string(), "second replan");
            n.depends_on = vec![TaskId(extra_task_id)];
            n
        };
        scheduler.graph.tasks[extra_task_id as usize].status = TaskStatus::Completed;
        scheduler
            .inject_tasks(TaskId(extra_task_id), vec![extra_task2], 50)
            .expect("second inject must succeed");

        let _ = scheduler.tick();
        assert_eq!(
            scheduler.max_parallel, expected_max_parallel,
            "max_parallel must not drift after second inject+tick"
        );
    }
}
