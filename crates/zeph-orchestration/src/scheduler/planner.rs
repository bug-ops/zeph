// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Scheduling decision logic: topology re-analysis, level barrier advancement,
//! and graph completion detection.

use super::{DagScheduler, SchedulerAction};
use crate::dag;
use crate::graph::{GraphStatus, TaskStatus};
use crate::topology::{
    DispatchStrategy, Topology, TopologyAnalysis, TopologyClassifier, build_rev_adj,
};

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
                    rev_adj: Vec::new(),
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
                let rev_adj = build_rev_adj(&self.graph.tasks);
                TopologyAnalysis {
                    topology: topo,
                    strategy,
                    max_parallel,
                    depth,
                    depths,
                    rev_adj,
                }
            }
        };
        self.topology = new_analysis;
        self.max_parallel = self.topology.max_parallel;
        self.topology_dirty = false;
        if self.topology.strategy == DispatchStrategy::LevelBarrier {
            // D4 (spec-075 FR-D-01): a Dormant route_to fallback is parked, not
            // blocking — exclude it from the min-active-depth floor, else a Dormant
            // node at a shallow depth pulls `current_level` back down after every
            // `inject_tasks` and re-serializes levels the barrier already passed.
            let min_active = self
                .graph
                .tasks
                .iter()
                .filter(|t| !t.status.is_terminal() && t.status != TaskStatus::Dormant)
                .filter_map(|t| self.topology.depths.get(&t.id).copied())
                .min();
            if let Some(min_depth) = min_active {
                self.current_level = self.current_level.min(min_depth);
            }
        }
    }

    /// Advance the `LevelBarrier` level when all tasks at the current level are terminal.
    ///
    /// A [`TaskStatus::Dormant`] task is treated as parked/non-blocking here (D4,
    /// spec-075 FR-D-01): `validate` forces a `route_to` target to depth 0
    /// (`depends_on.is_empty()`), and without this the barrier would never advance past
    /// a still-Dormant fallback sitting at level 0 while its (deeper) source is still
    /// running — a silent livelock invisible to the deadlock detector, since the
    /// gated-but-ready source never shows `ready_tasks()` as empty.
    /// [`super::DagScheduler::check_graph_completion`]'s `resolve_dormant_after_terminal`
    /// sweep is what eventually resolves a Dormant node still parked at graph
    /// completion time — this predicate only keeps the barrier itself from stalling on
    /// one before that sweep runs.
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
            task_depth != self.current_level
                || t.status.is_terminal()
                || t.status == TaskStatus::Dormant
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
                    d == self.current_level
                        && !t.status.is_terminal()
                        && t.status != TaskStatus::Dormant
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

        // Mode-2 completion-time resolution sweep (spec-075 FR-D-01): must run BEFORE
        // the `all_terminal`/deadlock checks below. A still-Dormant route_to fallback
        // is non-terminal and excluded from `ready_tasks()`, so without this sweep a
        // successful plan carrying an untriggered fallback would be misreported as a
        // scheduler deadlock. This is the quiescent-tick chokepoint: it runs whenever
        // `check_graph_completion` is reached with no Running tasks, covering every way
        // a route_to source can terminalize without rerouting (success, upstream-skip,
        // cancel) in one place. NOTE: this sweep does NOT run on the Abort/retry-
        // exhausted `graph.status = Failed` path — `tick()` returns before reaching
        // `check_graph_completion` on that path, so a Dormant fallback can persist into
        // a Failed graph. That is acceptable: `/plan retry` (`dag::reset_for_retry`)
        // re-arms it if its source is reset, or this sweep resolves it once the
        // retried graph heads to Completed.
        if !dag::resolve_dormant_after_terminal(&mut self.graph, &self.topology.rev_adj).is_empty()
        {
            self.graph_dirty = true;
        }
        let all_terminal = self.graph.tasks.iter().all(|t| t.status.is_terminal());
        if all_terminal {
            self.graph.status = GraphStatus::Completed;
            self.graph.finished_at = Some(crate::graph::chrono_now());
            self.graph_dirty = true;
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
            self.graph_dirty = true;
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
                last_progress_at: None,
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

    // --- Mode-2 route_to LevelBarrier tests (D4, spec-075 FR-D-01) ---
    //
    // A(0, depth0) -> B(1, depth1, route_to=F(2)). F(2, depth0, fallback, depends_on=[]).
    // `validate` forces a route_to target's `depends_on` empty, so F is always a graph
    // root — this graph naturally classifies as `Mixed` (two roots), not `Hierarchical`.
    // The LevelBarrier strategy and per-task depths are forced manually below (same
    // override pattern as `current_level` elsewhere in this file) to exercise the D4
    // barrier-parking predicates against a route_to source sitting deeper than its
    // depth-0 fallback — the exact shape the critic confirmed hangs without the fix.

    fn make_route_to_level_barrier_graph() -> crate::graph::TaskGraph {
        let mut g = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[]),
        ]);
        g.tasks[1].recovery = Some(crate::graph::RecoveryAction {
            state_injection: None,
            route_to: Some(crate::graph::TaskId(2)),
        });
        g
    }

    fn force_level_barrier_with_route_to_depths(scheduler: &mut DagScheduler) {
        use crate::graph::TaskId;
        use crate::topology::{DispatchStrategy, build_rev_adj};
        scheduler.topology.strategy = DispatchStrategy::LevelBarrier;
        scheduler.topology.depth = 1;
        scheduler.topology.depths = [(TaskId(0), 0), (TaskId(1), 1), (TaskId(2), 0)]
            .into_iter()
            .collect();
        scheduler.topology.rev_adj = build_rev_adj(&scheduler.graph.tasks);
        scheduler.current_level = 0;
    }

    #[test]
    fn test_level_barrier_route_to_source_succeeds_dormant_fallback_resolves_without_hang() {
        use crate::graph::TaskId;

        let graph = make_route_to_level_barrier_graph();
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
            scheduler.graph.tasks[2].status,
            TaskStatus::Dormant,
            "F must start Dormant"
        );

        force_level_barrier_with_route_to_depths(&mut scheduler);

        // Tick 1: only A (depth 0) dispatches. Dormant F sits at depth 0 too but must
        // not block dispatch or the barrier's advancement predicate.
        let actions = scheduler.tick();
        let spawned: Vec<_> = actions
            .iter()
            .filter_map(|a| {
                if let SchedulerAction::Spawn { task_id, .. } = a {
                    Some(*task_id)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(spawned, vec![TaskId(0)]);

        scheduler.graph.tasks[0].status = TaskStatus::Completed;
        scheduler.running.clear();

        // Tick 2: before the D4 fix, the still-Dormant F at level 0 would prevent the
        // barrier from ever advancing, so B (depth 1) would never dispatch, never fail,
        // and route_to would never fire -- a silent livelock invisible to the deadlock
        // detector (ready_tasks() is non-empty: B is ready but level-gated).
        let actions2 = scheduler.tick();
        assert_eq!(
            scheduler.current_level, 1,
            "barrier must advance past level 0 despite the Dormant F sitting there"
        );
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
        assert_eq!(
            spawned2,
            vec![TaskId(1)],
            "B must dispatch once the barrier advances"
        );

        // B completes without ever failing -> route_to never fires; F must resolve via
        // the completion-time sweep rather than strand the graph.
        scheduler.graph.tasks[1].status = TaskStatus::Completed;
        scheduler.running.clear();

        let actions3 = scheduler.tick();
        assert_eq!(
            scheduler.graph.tasks[2].status,
            TaskStatus::Skipped,
            "untriggered fallback must resolve Skipped via the completion sweep"
        );
        assert!(
            actions3.iter().any(|a| matches!(
                a,
                SchedulerAction::Done {
                    status: crate::graph::GraphStatus::Completed
                }
            )),
            "graph must complete, not deadlock, once the fallback resolves: {actions3:?}"
        );
    }

    #[test]
    fn test_level_barrier_route_to_source_fails_fallback_activates_out_of_level() {
        use crate::graph::TaskId;
        use crate::scheduler::{RunningTask, TaskEvent, TaskOutcome};

        let graph = make_route_to_level_barrier_graph();
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

        force_level_barrier_with_route_to_depths(&mut scheduler);

        // Advance to level 1: A dispatches and completes, B dispatches.
        scheduler.tick();
        scheduler.graph.tasks[0].status = TaskStatus::Completed;
        scheduler.running.clear();
        scheduler.tick();
        assert_eq!(scheduler.current_level, 1);
        assert_eq!(scheduler.graph.tasks[1].status, TaskStatus::Running);

        // B fails terminally.
        scheduler.running.insert(
            TaskId(1),
            RunningTask {
                agent_handle_id: "h1".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: std::time::Instant::now(),
                admission_permit: None,
                last_progress_at: None,
            },
        );
        scheduler.buffered_events.push_back(TaskEvent {
            task_id: TaskId(1),
            agent_handle_id: "h1".to_string(),
            outcome: TaskOutcome::Failed {
                error: "simulated failure".to_string(),
            },
        });

        // Tick: try_reroute activates F (Dormant -> Ready, routed_from = Some(B)); F
        // must dispatch on this same tick, bypassing the depth-0-vs-current_level(1+)
        // gate rather than waiting for the barrier to wind back down to level 0.
        let actions = scheduler.tick();
        assert_eq!(scheduler.graph.tasks[1].status, TaskStatus::Failed);
        assert_eq!(scheduler.graph.tasks[2].status, TaskStatus::Running);
        assert_eq!(
            scheduler.graph.tasks[2].routed_from,
            Some(TaskId(1)),
            "activated fallback must record its source"
        );
        assert!(
            actions.iter().any(
                |a| matches!(a, SchedulerAction::Spawn { task_id, .. } if *task_id == TaskId(2))
            ),
            "F must dispatch out-of-level on the same tick it is activated: {actions:?}"
        );
        assert_eq!(
            scheduler.graph.status,
            crate::graph::GraphStatus::Running,
            "graph must stay Running -- the failure was absorbed by the reroute"
        );

        // F completes -> graph reaches Completed with B terminal-Failed alongside it.
        scheduler.graph.tasks[2].status = TaskStatus::Completed;
        scheduler.running.clear();
        let actions2 = scheduler.tick();
        assert!(
            actions2.iter().any(|a| matches!(
                a,
                SchedulerAction::Done {
                    status: crate::graph::GraphStatus::Completed
                }
            )),
            "graph must complete once the activated fallback finishes: {actions2:?}"
        );
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
