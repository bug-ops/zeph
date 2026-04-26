// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Task injection, predicate outcome recording, and graph state mutations.

use super::DagScheduler;
use crate::error::OrchestrationError;
use crate::graph::{TaskId, TaskNode, TaskStatus};
use crate::scheduler::verifier_inject_tasks;

impl DagScheduler {
    /// Inject new tasks into the graph after a verify-replan cycle.
    ///
    /// Appends tasks and validates DAG acyclicity. Sets `topology_dirty=true` so
    /// topology is re-analyzed at the start of the next `tick()`. Does NOT
    /// re-analyze topology here (critic C2 — topology computed during injection
    /// would be stale by the next tick).
    ///
    /// Per-task replan cap: each task is limited to 1 replan (critic S2).
    /// Global hard cap: total replan count across the run is limited to `max_replans`.
    ///
    /// # Errors
    ///
    /// Returns `OrchestrationError::VerificationFailed` if the graph would exceed
    /// `max_tasks` or injection introduces a cycle.
    pub fn inject_tasks(
        &mut self,
        verified_task_id: TaskId,
        new_tasks: Vec<TaskNode>,
        max_tasks: usize,
    ) -> Result<(), OrchestrationError> {
        if new_tasks.is_empty() {
            return Ok(());
        }

        // Per-task replan limit: 1 replan per task (critic S2).
        let task_replan_count = self.task_replan_counts.entry(verified_task_id).or_insert(0);
        if *task_replan_count >= 1 {
            tracing::warn!(
                task_id = %verified_task_id,
                "per-task replan limit (1) reached, skipping replan injection"
            );
            return Ok(());
        }

        // Global hard cap (critic S2).
        if self.global_replan_count >= self.max_replans {
            tracing::warn!(
                global_replan_count = self.global_replan_count,
                max_replans = self.max_replans,
                "global replan limit reached, skipping replan injection"
            );
            return Ok(());
        }

        verifier_inject_tasks(&mut self.graph, new_tasks, max_tasks)?;

        *task_replan_count += 1;
        self.global_replan_count += 1;

        // Signal that topology needs re-analysis on the next tick (critic C2).
        self.topology_dirty = true;

        // Reset cascade failure counts — the graph has fundamentally changed (C13 fix).
        if let Some(ref mut det) = self.cascade_detector {
            det.reset();
        }

        // Reset lineage chains — injected tasks change the dependency topology, so
        // stale lineage chains no longer reflect the current graph structure.
        self.lineage_chains.clear();

        // Reset predicate reasons — predicate re-run history is invalidated when new
        // tasks are injected (graph topology fundamentally changed).
        self.predicate_reasons.clear();

        Ok(())
    }

    /// Record the outcome of a predicate evaluation for `task_id`.
    ///
    /// When the predicate **failed** and the re-run budget allows it, this method
    /// resets the task to `Ready` (incrementing `retry_count`) so the scheduler
    /// re-dispatches it on the next tick. The prior failure `reason` is stored in
    /// `predicate_reasons` so `build_task_prompt()` can augment the next prompt.
    ///
    /// When both `max_retries` and `max_predicate_replans` are exhausted, the
    /// failed predicate is recorded as-is and `inject_predicate_remediation()` is
    /// called to request a replan via the normal budget.
    ///
    /// Note: predicate state is in-memory only; restart re-evaluates any pending predicates.
    /// After a crash, `predicate_outcome.is_none()` causes the scheduler to re-emit
    /// `VerifyPredicate` on the next startup tick (idempotent by design).
    ///
    /// # Errors
    ///
    /// Returns `OrchestrationError::TaskNotFound` when `task_id` is out of bounds.
    pub fn record_predicate_outcome(
        &mut self,
        task_id: TaskId,
        outcome: crate::verify_predicate::PredicateOutcome,
        max_tasks: usize,
    ) -> Result<(), OrchestrationError> {
        if task_id.index() >= self.graph.tasks.len() {
            return Err(OrchestrationError::TaskNotFound(task_id.to_string()));
        }

        self.graph.tasks[task_id.index()].predicate_outcome = Some(outcome.clone());

        if outcome.passed {
            // Gate cleared — downstream tasks will be unblocked by ready_tasks() on next tick.
            tracing::debug!(task_id = %task_id, confidence = outcome.confidence, "predicate passed");
            return Ok(());
        }

        // Predicate failed — attempt a re-run if budgets allow.
        let task = &self.graph.tasks[task_id.index()];
        let predicate_rerun_count = task.predicate_rerun_count;

        if self.predicate_replans_used < self.max_predicate_replans {
            tracing::info!(
                task_id = %task_id,
                predicate_rerun_count,
                predicate_replans_used = self.predicate_replans_used,
                "predicate failed, scheduling re-run"
            );
            let task = &mut self.graph.tasks[task_id.index()];
            task.predicate_rerun_count += 1;
            task.result = None;
            task.predicate_outcome = None;
            task.status = TaskStatus::Ready;
            self.predicate_replans_used += 1;
            self.predicate_reasons.insert(task_id, outcome.reason);
            return Ok(());
        }

        // Budget exhausted — inject remediation task via regular replan budget.
        tracing::warn!(
            task_id = %task_id,
            predicate_rerun_count,
            predicate_replans_used = self.predicate_replans_used,
            max_predicate_replans = self.max_predicate_replans,
            "predicate re-run budget exhausted, injecting remediation task"
        );
        self.inject_predicate_remediation(task_id, &outcome.reason, max_tasks)?;
        Ok(())
    }

    /// Prior predicate failure reason for `task_id`, if any.
    ///
    /// Used by `build_task_prompt()` to augment the re-run prompt with context from the
    /// previous evaluation.
    pub fn predicate_failure_reason(&self, task_id: TaskId) -> Option<&str> {
        self.predicate_reasons.get(&task_id).map(String::as_str)
    }

    /// Inject a remediation task after predicate re-run budget is exhausted.
    ///
    /// Consumes the regular `max_replans` budget (same as verifier-driven replan)
    /// because remediation injects new tasks into the DAG.
    fn inject_predicate_remediation(
        &mut self,
        failed_task_id: TaskId,
        reason: &str,
        max_tasks: usize,
    ) -> Result<(), OrchestrationError> {
        // Per-task replan cap and global cap are both checked by inject_tasks().
        let task = &self.graph.tasks[failed_task_id.index()];
        let title = format!("Remediate: {}", task.title);
        let description = format!(
            "The output of task '{}' failed its verification predicate.\n\
             Reason: {reason}\n\n\
             Re-attempt the task with a corrected approach.",
            task.title
        );

        let task_idx = u32::try_from(self.graph.tasks.len()).map_err(|_| {
            OrchestrationError::VerificationFailed("task index overflows u32".to_string())
        })?;
        let mut remediation = crate::graph::TaskNode::new(task_idx, title, description);
        remediation.depends_on = vec![failed_task_id];
        remediation
            .agent_hint
            .clone_from(&self.graph.tasks[failed_task_id.index()].agent_hint);

        let replan_before = self.global_replan_count;
        self.inject_tasks(failed_task_id, vec![remediation], max_tasks)?;

        if self.global_replan_count == replan_before {
            // inject_tasks silently no-op'd because the global replan budget is exhausted.
            return Err(OrchestrationError::ReplanBudgetExhausted {
                task_id: failed_task_id.to_string(),
                reason: "predicate remediation".to_string(),
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{GraphStatus, TaskId, TaskResult, TaskStatus};
    use crate::scheduler::tests::*;

    // --- inject_tasks replan cap tests ---

    #[test]
    fn test_inject_tasks_per_task_cap_skips_second() {
        let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        let mut scheduler = make_scheduler(graph);

        let first = make_node(2, &[]);
        scheduler.inject_tasks(TaskId(0), vec![first], 20).unwrap();
        assert_eq!(scheduler.graph.tasks.len(), 3);
        assert_eq!(scheduler.global_replan_count, 1);

        let second = make_node(3, &[]);
        scheduler.inject_tasks(TaskId(0), vec![second], 20).unwrap();
        assert_eq!(
            scheduler.graph.tasks.len(),
            3,
            "second inject must be silently skipped (per-task cap)"
        );
        assert_eq!(scheduler.global_replan_count, 1);
    }

    #[test]
    fn test_inject_tasks_global_cap_skips_when_exhausted() {
        let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        let mut config = make_config();
        config.max_replans = 1;
        let defs = vec![make_def("worker")];
        let mut scheduler = DagScheduler::new(graph, &config, Box::new(FirstRouter), defs).unwrap();

        let new1 = make_node(2, &[]);
        scheduler.inject_tasks(TaskId(0), vec![new1], 20).unwrap();
        assert_eq!(scheduler.global_replan_count, 1);

        let new2 = make_node(3, &[]);
        scheduler.inject_tasks(TaskId(1), vec![new2], 20).unwrap();
        assert_eq!(scheduler.graph.tasks.len(), 3);
        assert_eq!(scheduler.global_replan_count, 1);
    }

    #[test]
    fn test_inject_tasks_sets_topology_dirty() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);
        assert!(!scheduler.topology_dirty);

        let new_task = make_node(1, &[]);
        scheduler
            .inject_tasks(TaskId(0), vec![new_task], 20)
            .unwrap();
        assert!(scheduler.topology_dirty);

        scheduler.tick();
        assert!(!scheduler.topology_dirty);
    }

    #[test]
    fn test_inject_tasks_rejects_cycle() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);

        let cyclic_task = make_node(1, &[1]);
        let result = scheduler.inject_tasks(TaskId(0), vec![cyclic_task], 20);
        assert!(result.is_err(), "cyclic injection must return an error");
        assert!(
            matches!(
                result.unwrap_err(),
                crate::error::OrchestrationError::VerificationFailed(_)
            ),
            "must return VerificationFailed for cycle"
        );
        assert_eq!(scheduler.global_replan_count, 0);
        assert!(!scheduler.topology_dirty);
    }

    #[test]
    fn inject_tasks_resets_cascade_detector() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[1].status = TaskStatus::Completed;
        let config = zeph_config::OrchestrationConfig {
            topology_selection: true,
            cascade_routing: true,
            cascade_failure_threshold: 0.4,
            max_parallel: 4,
            ..make_config()
        };
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        if let Some(ref mut det) = scheduler.cascade_detector {
            let g = &scheduler.graph;
            det.record_outcome(TaskId(1), false, g);
            assert_eq!(det.region_health().len(), 1);
        } else {
            panic!("cascade_detector must be Some");
        }

        let new_task = make_node(2, &[1]);
        scheduler
            .inject_tasks(TaskId(1), vec![new_task], 20)
            .unwrap();

        assert!(
            scheduler
                .cascade_detector
                .as_ref()
                .is_some_and(|d| d.region_health().is_empty()),
            "cascade_detector must be cleared after inject_tasks (C13 fix)"
        );
    }

    #[test]
    fn inject_tasks_resets_lineage_chains() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[1].status = TaskStatus::Completed;
        let mut config = make_config();
        config.cascade_chain_threshold = 3;
        config.lineage_ttl_secs = 300;
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        let mut chain = crate::lineage::ErrorLineage::default();
        chain.push(crate::lineage::LineageEntry {
            task_id: TaskId(0),
            kind: crate::lineage::LineageKind::Failed {
                error_class: "timeout".to_string(),
            },
            ts_ms: crate::lineage::now_ms(),
        });
        scheduler.lineage_chains.insert(TaskId(0), chain);
        assert!(!scheduler.lineage_chains.is_empty());

        let new_task = make_node(2, &[1]);
        scheduler
            .inject_tasks(TaskId(1), vec![new_task], 20)
            .unwrap();
        assert!(
            scheduler.lineage_chains.is_empty(),
            "lineage_chains must be cleared after inject_tasks"
        );
    }

    // --- VeriMAP predicate gate tests ---

    fn make_predicate_config() -> zeph_config::OrchestrationConfig {
        zeph_config::OrchestrationConfig {
            verify_predicate_enabled: true,
            max_predicate_replans: 2,
            ..make_config()
        }
    }

    fn make_predicate_scheduler(graph: crate::graph::TaskGraph) -> DagScheduler {
        let config = make_predicate_config();
        let defs = vec![make_def("worker")];
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs).unwrap()
    }

    #[test]
    fn predicate_gate_blocks_downstream_until_outcome_recorded() {
        use crate::scheduler::SchedulerAction;
        use crate::verify_predicate::VerifyPredicate;

        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[0].result = Some(TaskResult {
            output: "output".to_string(),
            artifacts: vec![],
            duration_ms: 10,
            agent_id: None,
            agent_def: None,
        });
        graph.tasks[0].verify_predicate =
            Some(VerifyPredicate::Natural("must be non-empty".to_string()));
        graph.tasks[1].status = TaskStatus::Pending;
        let mut scheduler = make_predicate_scheduler(graph);
        scheduler.graph.status = GraphStatus::Running;

        let actions = scheduler.tick();

        let has_verify = actions.iter().any(|a| {
            matches!(a, SchedulerAction::VerifyPredicate { task_id, .. } if *task_id == TaskId(0))
        });
        assert!(has_verify, "tick() must emit VerifyPredicate for task 0");

        let task1_spawned = actions.iter().any(|a| {
            matches!(a, SchedulerAction::Spawn { task_id, .. } if *task_id == TaskId(1))
                || matches!(a, SchedulerAction::RunInline { task_id, .. } if *task_id == TaskId(1))
        });
        assert!(
            !task1_spawned,
            "task 1 must not be dispatched while gate is open"
        );
    }

    #[test]
    fn predicate_pass_unblocks_downstream() {
        use crate::scheduler::SchedulerAction;
        use crate::verify_predicate::{PredicateOutcome, VerifyPredicate};

        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[0].result = Some(TaskResult {
            output: "output".to_string(),
            artifacts: vec![],
            duration_ms: 10,
            agent_id: None,
            agent_def: None,
        });
        graph.tasks[0].verify_predicate = Some(VerifyPredicate::Natural("criterion".to_string()));
        graph.tasks[1].status = TaskStatus::Pending;
        let mut scheduler = make_predicate_scheduler(graph);
        scheduler.graph.status = GraphStatus::Running;

        scheduler
            .record_predicate_outcome(
                TaskId(0),
                PredicateOutcome {
                    passed: true,
                    confidence: 0.9,
                    reason: "ok".to_string(),
                },
                20,
            )
            .unwrap();

        let actions = scheduler.tick();

        let task1_dispatched = actions.iter().any(|a| {
            matches!(a, SchedulerAction::Spawn { task_id, .. } if *task_id == TaskId(1))
                || matches!(a, SchedulerAction::RunInline { task_id, .. } if *task_id == TaskId(1))
        });
        assert!(
            task1_dispatched,
            "task 1 must be dispatched after predicate passed"
        );
    }

    #[test]
    fn predicate_fail_triggers_rerun_and_closes_gate() {
        use crate::verify_predicate::{PredicateOutcome, VerifyPredicate};

        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[0].result = Some(TaskResult {
            output: "bad".to_string(),
            artifacts: vec![],
            duration_ms: 10,
            agent_id: None,
            agent_def: None,
        });
        graph.tasks[0].verify_predicate =
            Some(VerifyPredicate::Natural("must be valid JSON".to_string()));
        graph.tasks[0].max_retries = Some(3);
        graph.tasks[1].status = TaskStatus::Pending;
        let mut scheduler = make_predicate_scheduler(graph);
        scheduler.graph.status = GraphStatus::Running;

        scheduler
            .record_predicate_outcome(
                TaskId(0),
                PredicateOutcome {
                    passed: false,
                    confidence: 0.1,
                    reason: "not JSON".to_string(),
                },
                20,
            )
            .unwrap();

        assert_eq!(
            scheduler.graph.tasks[0].status,
            TaskStatus::Ready,
            "failed predicate must reset task to Ready"
        );
        assert!(
            scheduler.graph.tasks[0].predicate_outcome.is_none(),
            "predicate_outcome must be None after re-run reset"
        );
        assert_eq!(scheduler.graph.tasks[0].predicate_rerun_count, 1);
        assert_eq!(scheduler.graph.tasks[0].retry_count, 0);
        let ready = crate::dag::ready_tasks(&scheduler.graph);
        assert!(!ready.contains(&TaskId(1)), "task 1 must remain gated");
    }

    #[test]
    fn predicate_budget_exhaustion_drops_rerun() {
        use crate::verify_predicate::{PredicateOutcome, VerifyPredicate};

        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[0].result = Some(TaskResult {
            output: "x".to_string(),
            artifacts: vec![],
            duration_ms: 1,
            agent_id: None,
            agent_def: None,
        });
        graph.tasks[0].verify_predicate = Some(VerifyPredicate::Natural("criterion".to_string()));
        graph.tasks[0].max_retries = Some(10);

        let mut config = make_predicate_config();
        config.max_predicate_replans = 0;
        let defs = vec![make_def("worker")];
        let mut scheduler = DagScheduler::new(graph, &config, Box::new(FirstRouter), defs).unwrap();
        scheduler.graph.status = GraphStatus::Running;

        let result = scheduler.record_predicate_outcome(
            TaskId(0),
            PredicateOutcome {
                passed: false,
                confidence: 0.0,
                reason: "nope".to_string(),
            },
            20,
        );
        assert!(result.is_ok());
        assert_ne!(scheduler.graph.tasks[0].status, TaskStatus::Ready);
    }

    #[test]
    fn verify_predicate_emit_is_idempotent_each_tick() {
        use crate::scheduler::SchedulerAction;
        use crate::verify_predicate::VerifyPredicate;

        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[0].result = Some(TaskResult {
            output: "out".to_string(),
            artifacts: vec![],
            duration_ms: 1,
            agent_id: None,
            agent_def: None,
        });
        graph.tasks[0].verify_predicate = Some(VerifyPredicate::Natural("check".to_string()));
        graph.tasks[1].status = TaskStatus::Pending;
        let mut scheduler = make_predicate_scheduler(graph);
        scheduler.graph.status = GraphStatus::Running;

        let actions1 = scheduler.tick();
        let count1 = actions1
            .iter()
            .filter(|a| matches!(a, SchedulerAction::VerifyPredicate { .. }))
            .count();
        assert_eq!(count1, 1, "first tick must emit exactly 1 VerifyPredicate");

        let actions2 = scheduler.tick();
        let count2 = actions2
            .iter()
            .filter(|a| matches!(a, SchedulerAction::VerifyPredicate { .. }))
            .count();
        assert_eq!(
            count2, 1,
            "second tick must re-emit VerifyPredicate (idempotent)"
        );
    }

    #[test]
    fn record_predicate_outcome_out_of_bounds_returns_task_not_found() {
        use crate::verify_predicate::{PredicateOutcome, VerifyPredicate};

        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].verify_predicate = Some(VerifyPredicate::Natural("criterion".to_string()));
        graph.tasks[0].status = TaskStatus::Completed;
        let mut scheduler = make_predicate_scheduler(graph);
        scheduler.graph.status = GraphStatus::Running;

        let out_of_bounds = TaskId(99);
        let outcome = PredicateOutcome {
            passed: true,
            confidence: 1.0,
            reason: "ok".to_string(),
        };
        let err = scheduler
            .record_predicate_outcome(out_of_bounds, outcome, 64)
            .unwrap_err();
        assert!(
            matches!(err, crate::error::OrchestrationError::TaskNotFound(_)),
            "expected TaskNotFound, got {err:?}"
        );
    }

    #[test]
    fn predicate_remediation_returns_budget_exhausted_when_global_limit_reached() {
        use crate::verify_predicate::{PredicateOutcome, VerifyPredicate};

        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[0].result = Some(TaskResult {
            output: "x".to_string(),
            artifacts: vec![],
            duration_ms: 1,
            agent_id: None,
            agent_def: None,
        });
        graph.tasks[0].verify_predicate = Some(VerifyPredicate::Natural("criterion".to_string()));
        graph.tasks[0].max_retries = Some(10);

        let mut config = make_predicate_config();
        config.max_predicate_replans = 0;
        config.max_replans = 0;
        let defs = vec![make_def("worker")];
        let mut scheduler = DagScheduler::new(graph, &config, Box::new(FirstRouter), defs).unwrap();
        scheduler.graph.status = GraphStatus::Running;

        let result = scheduler.record_predicate_outcome(
            TaskId(0),
            PredicateOutcome {
                passed: false,
                confidence: 0.0,
                reason: "nope".to_string(),
            },
            20,
        );
        assert!(
            matches!(
                result,
                Err(crate::error::OrchestrationError::ReplanBudgetExhausted { .. })
            ),
            "expected ReplanBudgetExhausted, got {result:?}"
        );
    }

    // --- VMAO adaptive replanning accessor tests ---

    #[test]
    fn completeness_threshold_returns_config_value() {
        let mut config = make_config();
        config.completeness_threshold = 0.85;
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let scheduler =
            DagScheduler::new(graph, &config, Box::new(FirstRouter), vec![make_def("w")]).unwrap();
        assert!((scheduler.completeness_threshold() - 0.85).abs() < f32::EPSILON);
    }

    #[test]
    fn completeness_threshold_default_is_0_7() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let scheduler = make_scheduler(graph);
        assert!((scheduler.completeness_threshold() - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn verify_provider_name_returns_config_value() {
        let mut config = make_config();
        config.verify_provider = zeph_config::ProviderName::new("fast");
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let scheduler =
            DagScheduler::new(graph, &config, Box::new(FirstRouter), vec![make_def("w")]).unwrap();
        assert_eq!(scheduler.verify_provider_name(), "fast");
    }

    #[test]
    fn verify_provider_name_empty_when_not_set() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let scheduler = make_scheduler(graph);
        assert_eq!(scheduler.verify_provider_name(), "");
    }

    #[test]
    fn max_replans_remaining_initial_equals_max_replans() {
        let mut config = make_config();
        config.max_replans = 3;
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let scheduler =
            DagScheduler::new(graph, &config, Box::new(FirstRouter), vec![make_def("w")]).unwrap();
        assert_eq!(scheduler.max_replans_remaining(), 3);
    }

    #[test]
    fn max_replans_remaining_decrements_after_record() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);
        assert_eq!(scheduler.max_replans_remaining(), 2);
        scheduler.record_whole_plan_replan();
        assert_eq!(scheduler.max_replans_remaining(), 1);
        scheduler.record_whole_plan_replan();
        assert_eq!(scheduler.max_replans_remaining(), 0);
        scheduler.record_whole_plan_replan();
        assert_eq!(scheduler.max_replans_remaining(), 0);
    }

    #[test]
    fn record_whole_plan_replan_does_not_modify_graph() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);
        let task_count_before = scheduler.graph().tasks.len();
        scheduler.record_whole_plan_replan();
        assert_eq!(scheduler.graph().tasks.len(), task_count_before);
    }

    // --- #2238: validate_verify_config tests ---

    fn make_verify_config(provider: &str) -> zeph_config::OrchestrationConfig {
        zeph_config::OrchestrationConfig {
            verify_completeness: true,
            verify_provider: zeph_config::ProviderName::new(provider),
            ..make_config()
        }
    }

    #[test]
    fn validate_verify_config_unknown_provider_returns_err() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let config = make_verify_config("nonexistent");
        let scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();
        let result = scheduler.validate_verify_config(&["fast", "quality"]);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("nonexistent"));
        assert!(err_msg.contains("fast"));
    }

    #[test]
    fn validate_verify_config_known_provider_returns_ok() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let config = make_verify_config("fast");
        let scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();
        assert!(
            scheduler
                .validate_verify_config(&["fast", "quality"])
                .is_ok()
        );
    }

    #[test]
    fn validate_verify_config_empty_provider_always_ok() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let config = make_verify_config("");
        let scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();
        assert!(scheduler.validate_verify_config(&["fast"]).is_ok());
    }

    #[test]
    fn validate_verify_config_disabled_skips_validation() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let scheduler = make_scheduler(graph);
        assert!(scheduler.validate_verify_config(&["fast"]).is_ok());
    }

    #[test]
    fn validate_verify_config_empty_pool_skips_validation() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let config = make_verify_config("nonexistent");
        let scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();
        assert!(scheduler.validate_verify_config(&[]).is_ok());
    }

    #[test]
    fn validate_verify_config_trims_whitespace_in_config() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let config = make_verify_config("  fast  ");
        let scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();
        assert!(scheduler.validate_verify_config(&["fast"]).is_ok());
    }

    // --- resume_from tests ---

    #[test]
    fn test_resume_from_accepts_paused_graph() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.status = GraphStatus::Paused;
        graph.tasks[0].status = TaskStatus::Pending;

        let scheduler =
            DagScheduler::resume_from(graph, &make_config(), Box::new(FirstRouter), vec![])
                .expect("resume_from should accept Paused graph");
        assert_eq!(scheduler.graph.status, GraphStatus::Running);
    }

    #[test]
    fn test_resume_from_accepts_failed_graph() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.status = GraphStatus::Failed;
        graph.tasks[0].status = TaskStatus::Failed;

        let scheduler =
            DagScheduler::resume_from(graph, &make_config(), Box::new(FirstRouter), vec![])
                .expect("resume_from should accept Failed graph");
        assert_eq!(scheduler.graph.status, GraphStatus::Running);
    }

    #[test]
    fn test_resume_from_rejects_completed_graph() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.status = GraphStatus::Completed;

        let err = DagScheduler::resume_from(graph, &make_config(), Box::new(FirstRouter), vec![])
            .unwrap_err();
        assert!(matches!(
            err,
            crate::error::OrchestrationError::InvalidGraph(_)
        ));
    }

    #[test]
    fn test_resume_from_rejects_canceled_graph() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.status = GraphStatus::Canceled;

        let err = DagScheduler::resume_from(graph, &make_config(), Box::new(FirstRouter), vec![])
            .unwrap_err();
        assert!(matches!(
            err,
            crate::error::OrchestrationError::InvalidGraph(_)
        ));
    }

    #[test]
    fn test_resume_from_reconstructs_running_tasks() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.status = GraphStatus::Paused;
        graph.tasks[0].status = TaskStatus::Running;
        graph.tasks[0].assigned_agent = Some("handle-abc".to_string());
        graph.tasks[0].agent_hint = Some("worker".to_string());
        graph.tasks[1].status = TaskStatus::Pending;

        let scheduler =
            DagScheduler::resume_from(graph, &make_config(), Box::new(FirstRouter), vec![])
                .expect("should succeed");

        assert!(
            scheduler.running.contains_key(&TaskId(0)),
            "Running task must be reconstructed in the running map (IC1)"
        );
        assert_eq!(scheduler.running[&TaskId(0)].agent_handle_id, "handle-abc");
        assert!(
            !scheduler.running.contains_key(&TaskId(1)),
            "Pending task must not appear in running map"
        );
    }

    #[test]
    fn test_resume_from_sets_status_running() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.status = GraphStatus::Paused;

        let scheduler =
            DagScheduler::resume_from(graph, &make_config(), Box::new(FirstRouter), vec![])
                .unwrap();
        assert_eq!(scheduler.graph.status, GraphStatus::Running);
    }
}
