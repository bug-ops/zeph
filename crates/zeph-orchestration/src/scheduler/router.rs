// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Task routing and prompt construction.

use std::fmt::Write as _;

use super::DagScheduler;
use crate::graph::{TaskNode, TaskStatus};
use zeph_sanitizer::{ContentSource, ContentSourceKind};

impl DagScheduler {
    /// Build the task prompt with dependency context injection (Section 14).
    ///
    /// Uses char-boundary-safe truncation (S1 fix) to avoid panics on multi-byte UTF-8.
    /// Dependency output is sanitized (SEC-ORCH-01) and titles are XML-escaped to prevent
    /// prompt injection via crafted task outputs.
    pub(super) fn build_task_prompt(&self, task: &TaskNode) -> String {
        if task.depends_on.is_empty() {
            return task.description.clone();
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
                let source = ContentSource::new(ContentSourceKind::A2aMessage);
                let sanitized = self.sanitizer.sanitize(&result.output, source);
                let safe_output = sanitized.body;

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

/// Escape XML special characters in a string to prevent tag injection.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{TaskResult, TaskStatus};
    use crate::scheduler::tests::*;

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
}
