// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Task routing and prompt construction.

use std::fmt::Write as _;

use super::DagScheduler;
use crate::graph::{TaskNode, TaskStatus};
use zeph_common::text::xml_escape;

impl DagScheduler {
    /// Build the task prompt with dependency context injection (Section 14).
    ///
    /// Uses char-boundary-safe truncation (S1 fix) to avoid panics on multi-byte UTF-8.
    /// Dependency output is sanitized (SEC-ORCH-01) and titles are XML-escaped to prevent
    /// prompt injection via crafted task outputs.
    ///
    /// Mode-2 `route_to` injection (spec-075 FR-D-01): when `task.routed_from` is set,
    /// prepends a `<recovery-source>` block with the failed source's sanitized output.
    /// This runs **before** the empty-`depends_on` early return below, because a
    /// `route_to` target's `depends_on` is always empty (`validate` invariant) — it has
    /// no `Completed`-dependency channel to the source, so the `routed_from` marker is
    /// the sole path for the source's output to reach this prompt.
    pub(super) fn build_task_prompt(&self, task: &TaskNode) -> String {
        let recovery_block = task.routed_from.map(|src_id| {
            let src = &self.graph.tasks[src_id.index()];
            let escaped_id = xml_escape(&src.id.to_string());
            let escaped_title = xml_escape(&src.title);
            let safe_output = src
                .result
                .as_ref()
                .map_or_else(String::new, |r| self.sanitizer.sanitize_task_output(&r.output));
            format!(
                "<recovery-source>\n## Task \"{escaped_id}\": \"{escaped_title}\" (failed; this task is the recovery fallback)\n{safe_output}\n</recovery-source>\n\n"
            )
        });

        if task.depends_on.is_empty() {
            return match recovery_block {
                Some(block) => format!("{block}Your task: {}", task.description),
                None => task.description.clone(),
            };
        }

        let completed_deps: Vec<&TaskNode> = task
            .depends_on
            .iter()
            .filter_map(|dep_id| {
                let dep = &self.graph.tasks[dep_id.index()];
                if dep.status == TaskStatus::Completed {
                    Some(dep)
                } else {
                    None
                }
            })
            .collect();

        if completed_deps.is_empty() {
            return task.description.clone();
        }

        let budget_per_dep = self
            .dependency_context_budget
            .checked_div(completed_deps.len())
            .unwrap_or(self.dependency_context_budget);

        let mut context_block = String::from("<completed-dependencies>\n");

        for dep in &completed_deps {
            // SEC-ORCH-01: XML-escape dep.id and dep.title to prevent breaking out of the
            // <completed-dependencies> wrapper via crafted titles.
            let escaped_id = xml_escape(&dep.id.to_string());
            let escaped_title = xml_escape(&dep.title);
            let _ = writeln!(
                context_block,
                "## Task \"{escaped_id}\": \"{escaped_title}\" (completed)",
            );

            if let Some(ref result) = dep.result {
                // SEC-ORCH-01: sanitize dep output to prevent prompt injection from upstream tasks.
                let safe_output = self.sanitizer.sanitize_task_output(&result.output);

                // Char-boundary-safe truncation (S1): use chars().take() instead of byte slicing.
                let char_count = safe_output.chars().count();
                if char_count > budget_per_dep {
                    let truncated: String = safe_output.chars().take(budget_per_dep).collect();
                    let _ = write!(
                        context_block,
                        "{truncated}...\n[truncated: {char_count} chars total]"
                    );
                } else {
                    context_block.push_str(&safe_output);
                }
            } else {
                context_block.push_str("[no output recorded]\n");
            }
            context_block.push('\n');
        }

        // Add notes for skipped deps.
        for dep_id in &task.depends_on {
            let dep = &self.graph.tasks[dep_id.index()];
            if dep.status == TaskStatus::Skipped {
                let escaped_id = xml_escape(&dep.id.to_string());
                let escaped_title = xml_escape(&dep.title);
                let _ = writeln!(
                    context_block,
                    "## Task \"{escaped_id}\": \"{escaped_title}\" (skipped -- no output available)\n",
                );
            }
        }

        context_block.push_str("</completed-dependencies>\n\n");
        format!("{context_block}Your task: {}", task.description)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::propagate_failure;
    use crate::graph::{
        FailureStrategy, GraphStatus, RecoveryAction, TaskId, TaskResult, TaskStatus,
    };
    use crate::scheduler::tests::*;
    use crate::scheduler::{RunningTask, SchedulerAction, TaskEvent, TaskOutcome};
    use crate::topology::build_rev_adj;

    #[test]
    fn test_build_prompt_no_deps() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let scheduler = make_scheduler(graph);
        let prompt = scheduler.build_task_prompt(&scheduler.graph.tasks[0]);
        assert_eq!(prompt, "description for task 0");
    }

    #[test]
    fn test_build_prompt_with_deps_and_truncation() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Completed;
        // Create output longer than budget
        graph.tasks[0].result = Some(TaskResult {
            output: "x".repeat(200),
            artifacts: vec![],
            duration_ms: 10,
            agent_id: None,
            agent_def: None,
        });

        let config = zeph_config::OrchestrationConfig {
            dependency_context_budget: 50,
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

        let prompt = scheduler.build_task_prompt(&scheduler.graph.tasks[1]);
        assert!(prompt.contains("<completed-dependencies>"));
        assert!(prompt.contains("[truncated:"));
        assert!(prompt.contains("Your task:"));
    }

    #[test]
    fn test_utf8_safe_truncation() {
        // S1 regression: truncation must not panic on multi-byte UTF-8.
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Completed;
        let unicode_output = "日本語テスト".repeat(100);
        graph.tasks[0].result = Some(TaskResult {
            output: unicode_output,
            artifacts: vec![],
            duration_ms: 10,
            agent_id: None,
            agent_def: None,
        });

        let config = zeph_config::OrchestrationConfig {
            dependency_context_budget: 500,
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

        let prompt = scheduler.build_task_prompt(&scheduler.graph.tasks[1]);
        assert!(
            prompt.contains("日"),
            "Japanese characters should be in the prompt after safe truncation"
        );
    }

    #[test]
    fn test_build_prompt_chars_count_in_truncation_message() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Completed;
        let output = "x".repeat(200);
        graph.tasks[0].result = Some(TaskResult {
            output,
            artifacts: vec![],
            duration_ms: 10,
            agent_id: None,
            agent_def: None,
        });

        let config = zeph_config::OrchestrationConfig {
            dependency_context_budget: 10,
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

        let prompt = scheduler.build_task_prompt(&scheduler.graph.tasks[1]);
        assert!(
            prompt.contains("chars total"),
            "truncation message must use 'chars total' label. Prompt: {prompt}"
        );
        assert!(
            prompt.contains("[truncated:"),
            "prompt must contain truncation notice. Prompt: {prompt}"
        );
    }

    #[test]
    fn test_build_prompt_includes_mode1_recovered_state_injection() {
        // Drives the real Mode-1 recovery path (propagate_failure -> try_recover in dag.rs)
        // end-to-end, rather than hand-constructing the recovered TaskNode, so this test
        // breaks if try_recover()'s field mapping or completion marker ever changes.
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.status = GraphStatus::Running;
        graph.tasks[0].status = TaskStatus::Failed;
        graph.tasks[0].failure_strategy = Some(FailureStrategy::Abort);
        graph.tasks[0].recovery = Some(RecoveryAction {
            state_injection: Some("fallback output".to_string()),
            route_to: None,
        });

        let rev_adj = build_rev_adj(&graph.tasks);
        let to_cancel = propagate_failure(&mut graph, TaskId(0), &rev_adj);
        assert!(to_cancel.is_empty());
        assert_eq!(graph.tasks[0].status, TaskStatus::Completed);

        // DagScheduler::new independently requires a freshly-`Created` graph; recovery leaves
        // `graph.status` untouched (`Running`), so reset it here purely to satisfy that
        // unrelated constructor invariant -- it does not affect the recovered task state above.
        graph.status = GraphStatus::Created;

        let config = make_config();
        let scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
            None,
        )
        .unwrap();

        let prompt = scheduler.build_task_prompt(&scheduler.graph.tasks[1]);
        assert!(
            prompt.contains("fallback output"),
            "dependent's prompt must include the recovered dependency's synthetic output. Prompt: {prompt}"
        );
        assert!(
            !prompt.contains("__recovery__"),
            "agent_def marker must not leak into the prompt. Prompt: {prompt}"
        );
    }

    #[test]
    fn test_build_prompt_includes_mode2_routed_from_injection() {
        // Drives the real Mode-2 reroute path (propagate_failure -> try_reroute in
        // dag.rs) end-to-end: F(1) is a route_to target for B(0), which fails.
        use crate::graph::TaskStatus as TS;

        // B=0 (source), F=1 (target, empty depends_on).
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
        graph.tasks[0].recovery = Some(RecoveryAction {
            state_injection: None,
            route_to: Some(TaskId(1)),
        });
        graph.status = GraphStatus::Running;
        graph.tasks[1].status = TS::Dormant;
        graph.tasks[0].status = TS::Failed;
        graph.tasks[0].failure_strategy = Some(FailureStrategy::Abort);
        graph.tasks[0].result = Some(TaskResult {
            output: "boom: connection refused".to_string(),
            artifacts: vec![],
            duration_ms: 5,
            agent_id: None,
            agent_def: None,
        });

        let rev_adj = build_rev_adj(&graph.tasks);
        let to_cancel = propagate_failure(&mut graph, TaskId(0), &rev_adj);
        assert!(to_cancel.is_empty());
        assert_eq!(graph.tasks[1].status, TS::Ready);
        assert_eq!(graph.tasks[1].routed_from, Some(TaskId(0)));

        graph.status = GraphStatus::Created;
        let config = make_config();
        let scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
            None,
        )
        .unwrap();

        let prompt = scheduler.build_task_prompt(&scheduler.graph.tasks[1]);
        assert!(
            prompt.contains("<recovery-source>"),
            "prompt must include the recovery-source block. Prompt: {prompt}"
        );
        assert!(
            prompt.contains("boom: connection refused"),
            "prompt must include the failed source's sanitized output. Prompt: {prompt}"
        );
        assert!(
            prompt.contains("Your task:"),
            "prompt must still include the target's own task description. Prompt: {prompt}"
        );
    }

    #[test]
    fn test_build_prompt_mode2_routed_from_injection_via_real_failure_path() {
        // Regression for the reviewer's Critical finding: the test above manually
        // pre-sets `graph.tasks[0].result` before calling `propagate_failure`, a
        // precondition that never arises via the real dispatch pipeline -- production
        // Failed-transition sites (`tick/mod.rs`'s spawn-failure, `handle_failed_outcome`,
        // and timeout paths) never populated `.result`, so this test masked the gap where
        // Mechanism 4's injection was silently inert. This test instead drives the real
        // `handle_failed_outcome` path end-to-end via `tick()`, exactly as a live agent
        // failure event would, and asserts the fallback's prompt actually contains the
        // failed source's error content.
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
        graph.tasks[0].recovery = Some(RecoveryAction {
            state_injection: None,
            route_to: Some(TaskId(1)),
        });
        graph.tasks[0].failure_strategy = Some(FailureStrategy::Abort);

        let mut scheduler = make_scheduler(graph);
        assert_eq!(
            scheduler.graph.tasks[1].status,
            TaskStatus::Dormant,
            "route_to target must start Dormant"
        );

        // Simulate task 0 having been dispatched and now failing for real, via the same
        // event pipeline a live sub-agent failure uses.
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
                error: "boom: connection refused".to_string(),
            },
        });
        // A single `tick()` both processes the failure event (activating the Dormant
        // fallback via `try_reroute`) and dispatches it (it is now Ready with no
        // dependencies), so the returned `Spawn` action carries the exact prompt a live
        // sub-agent would receive -- built by the same `build_task_prompt` call this test
        // is regression-testing.
        let actions = scheduler.tick();

        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Failed);
        assert_eq!(scheduler.graph.tasks[1].routed_from, Some(TaskId(0)));

        let prompt = actions
            .iter()
            .find_map(|a| match a {
                SchedulerAction::Spawn {
                    task_id, prompt, ..
                } if *task_id == TaskId(1) => Some(prompt),
                _ => None,
            })
            .expect("fallback task must have been dispatched with a Spawn action this tick");
        assert!(
            prompt.contains("<recovery-source>"),
            "prompt must include the recovery-source block. Prompt: {prompt}"
        );
        assert!(
            prompt.contains("boom: connection refused"),
            "prompt must include the failed source's real error output, populated by the \
             production Failed-transition path in tick/mod.rs -- not a manually-set result. \
             Prompt: {prompt}"
        );
    }
}
