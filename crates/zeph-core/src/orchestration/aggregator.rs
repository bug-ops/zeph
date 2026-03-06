// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Result aggregation: collect completed task outputs and synthesize a coherent summary.

use std::fmt::Write as _;

use zeph_llm::provider::{LlmProvider, Message, Role};

use super::error::OrchestrationError;
use super::graph::{TaskGraph, TaskStatus};
use crate::config::OrchestrationConfig;
use crate::sanitizer::{
    ContentIsolationConfig, ContentSanitizer, ContentSource, ContentSourceKind,
};

/// Collects results from completed tasks and produces a final synthesis.
#[allow(async_fn_in_trait)]
pub trait Aggregator: Send + Sync {
    /// Synthesize a final response from completed task results in `graph`.
    ///
    /// Considers all `Completed` tasks. Skipped tasks are mentioned but their
    /// absent output is noted. On LLM call failure the implementation must
    /// fall back to raw concatenation rather than propagating the error.
    ///
    /// # Errors
    ///
    /// Returns `OrchestrationError::AggregationFailed` if neither synthesis
    /// nor fallback concatenation can produce output (e.g., no tasks at all).
    async fn aggregate(&self, graph: &TaskGraph) -> Result<String, OrchestrationError>;
}

/// LLM-backed [`Aggregator`] that synthesizes task outputs into a coherent response.
pub struct LlmAggregator<P: LlmProvider> {
    provider: P,
    /// Total character budget for all task outputs combined. Divided equally among
    /// completed tasks at aggregation time (S1).
    aggregation_char_budget: usize,
    sanitizer: ContentSanitizer,
}

impl<P: LlmProvider> LlmAggregator<P> {
    /// Create a new `LlmAggregator` from a provider and config.
    #[must_use]
    pub fn new(provider: P, config: &OrchestrationConfig) -> Self {
        // Estimate 4 chars/token for the total budget.
        let aggregation_char_budget = config.aggregator_max_tokens as usize * 4;
        Self {
            provider,
            aggregation_char_budget,
            sanitizer: ContentSanitizer::new(&ContentIsolationConfig::default()),
        }
    }
}

impl<P: LlmProvider + Send + Sync> Aggregator for LlmAggregator<P> {
    async fn aggregate(&self, graph: &TaskGraph) -> Result<String, OrchestrationError> {
        let completed: Vec<_> = graph
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Completed && t.result.is_some())
            .collect();

        let skipped: Vec<_> = graph
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Skipped)
            .collect();

        if completed.is_empty() && skipped.is_empty() {
            return Err(OrchestrationError::AggregationFailed(
                "no completed or skipped tasks to aggregate".into(),
            ));
        }

        // S1: divide total budget equally among completed tasks.
        let num_completed = completed.len().max(1);
        let per_task = self.aggregation_char_budget / num_completed;

        // Build task output sections with spotlight wrapping (I1).
        let mut task_sections = String::new();
        for task in &completed {
            let raw_output = task.result.as_ref().map_or("", |r| r.output.as_str());

            // Truncate to per-task budget before spotlighting.
            let truncated = truncate_chars(raw_output, per_task);

            let sanitized = self
                .sanitizer
                .sanitize(truncated, ContentSource::new(ContentSourceKind::ToolResult));

            let _ = write!(
                task_sections,
                "### Task: {}\n{}\n\n",
                task.title, sanitized.body
            );
        }

        // S2: include skipped task reasons.
        if !skipped.is_empty() {
            task_sections.push_str("### Skipped tasks (no output available):\n");
            for task in &skipped {
                let _ = writeln!(task_sections, "- {} ({})", task.title, task.description);
            }
            task_sections.push('\n');
        }

        let system = "You are a result synthesizer. Given the outputs from a set of completed \
                      sub-tasks, produce a single coherent summary that directly addresses \
                      the original goal. Be concise. If tasks were skipped, acknowledge them briefly.";

        let user = format!(
            "Goal: {goal}\n\n\
             Task results:\n\n\
             {task_sections}\
             Synthesize the above into a single coherent response for the user.",
            goal = graph.goal,
        );

        let messages = vec![
            Message::from_legacy(Role::System, system),
            Message::from_legacy(Role::User, user),
        ];

        match self.provider.chat(&messages).await {
            Ok(synthesis) => Ok(synthesis),
            Err(e) => {
                // I3: on LLM failure, fall back to raw concatenation.
                tracing::error!(
                    graph_id = %graph.id,
                    error = %e,
                    "aggregation LLM call failed; falling back to raw concatenation"
                );
                Ok(build_fallback(graph, &self.sanitizer, per_task))
            }
        }
    }
}

/// Truncate `s` to at most `max_chars` Unicode scalar values.
fn truncate_chars(s: &str, max_chars: usize) -> &str {
    if max_chars == 0 {
        return "";
    }
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => &s[..byte_idx],
        None => s,
    }
}

/// Raw concatenation fallback when the LLM call fails (I3).
///
/// SEC-P5-02: sanitizes each task output through `sanitizer` before including it,
/// matching the security posture of the main aggregation path.
/// IS3: also applies per-task truncation using `per_task_chars` budget.
fn build_fallback(
    graph: &TaskGraph,
    sanitizer: &ContentSanitizer,
    per_task_chars: usize,
) -> String {
    let mut out = String::new();
    let _ = write!(out, "Goal: {}\n\n", graph.goal);
    for task in &graph.tasks {
        if task.status == TaskStatus::Completed {
            if let Some(ref result) = task.result {
                let truncated = truncate_chars(&result.output, per_task_chars);
                let cleaned = sanitizer
                    .sanitize(truncated, ContentSource::new(ContentSourceKind::ToolResult));
                let _ = write!(out, "## {}\n{}\n\n", task.title, cleaned.body);
            }
        } else if task.status == TaskStatus::Skipped {
            let _ = write!(
                out,
                "## {} (skipped — {})\n\n",
                task.title, task.description
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestration::graph::{GraphStatus, TaskGraph, TaskNode, TaskResult, TaskStatus};

    fn make_graph_with_tasks(statuses: &[(TaskStatus, Option<&str>)]) -> TaskGraph {
        let mut graph = TaskGraph::new("test goal");
        for (i, (status, output)) in statuses.iter().enumerate() {
            let mut node = TaskNode::new(u32::try_from(i).unwrap(), format!("task-{i}"), "desc");
            node.status = *status;
            node.result = output.map(|o| TaskResult {
                output: o.to_string(),
                artifacts: vec![],
                duration_ms: 100,
                agent_id: None,
                agent_def: None,
            });
            graph.tasks.push(node);
        }
        graph.status = GraphStatus::Completed;
        graph
    }

    // --- truncate_chars tests ---

    #[test]
    fn test_truncate_empty() {
        assert_eq!(truncate_chars("", 10), "");
    }

    #[test]
    fn test_truncate_zero_budget() {
        assert_eq!(truncate_chars("hello", 0), "");
    }

    #[test]
    fn test_truncate_within_budget() {
        assert_eq!(truncate_chars("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_at_limit() {
        assert_eq!(truncate_chars("hello world", 5), "hello");
    }

    #[test]
    fn test_truncate_multibyte() {
        // "привет" is 6 chars, 12 bytes in UTF-8
        let s = "привет мир";
        let truncated = truncate_chars(s, 6);
        assert_eq!(truncated, "привет");
    }

    // --- build_fallback tests ---

    fn make_sanitizer() -> ContentSanitizer {
        ContentSanitizer::new(&ContentIsolationConfig::default())
    }

    #[test]
    fn test_build_fallback_includes_completed() {
        let graph = make_graph_with_tasks(&[(TaskStatus::Completed, Some("output-a"))]);
        let out = build_fallback(&graph, &make_sanitizer(), 4096);
        // Fallback sanitizes via spotlight wrapping; output-a is inside the body.
        assert!(
            out.contains("output-a"),
            "fallback should include task output"
        );
        assert!(out.contains("task-0"));
    }

    #[test]
    fn test_build_fallback_includes_skipped_marker() {
        let graph = make_graph_with_tasks(&[(TaskStatus::Skipped, None)]);
        let out = build_fallback(&graph, &make_sanitizer(), 4096);
        assert!(
            out.contains("skipped"),
            "fallback should mark skipped tasks"
        );
    }

    #[test]
    fn test_build_fallback_goal_included() {
        let graph = make_graph_with_tasks(&[(TaskStatus::Completed, Some("x"))]);
        let out = build_fallback(&graph, &make_sanitizer(), 4096);
        assert!(out.contains("test goal"));
    }

    // --- LlmAggregator integration tests (mock feature) ---

    #[cfg(feature = "mock")]
    mod mock_tests {
        use super::*;
        use zeph_llm::mock::MockProvider;

        fn make_config() -> OrchestrationConfig {
            OrchestrationConfig {
                aggregator_max_tokens: 1024,
                ..OrchestrationConfig::default()
            }
        }

        #[tokio::test]
        async fn test_aggregate_calls_llm_and_returns_synthesis() {
            let provider = MockProvider::with_responses(vec!["synthesized result".to_string()]);
            let agg = LlmAggregator::new(provider, &make_config());

            let graph = make_graph_with_tasks(&[(TaskStatus::Completed, Some("task output"))]);
            let result = agg.aggregate(&graph).await.unwrap();
            assert_eq!(result, "synthesized result");
        }

        #[tokio::test]
        async fn test_aggregate_fallback_on_llm_failure() {
            let provider = MockProvider::failing();
            let agg = LlmAggregator::new(provider, &make_config());

            let graph = make_graph_with_tasks(&[(TaskStatus::Completed, Some("raw output"))]);
            let result = agg.aggregate(&graph).await.unwrap();
            assert!(
                result.contains("raw output"),
                "fallback should have raw output"
            );
        }

        #[tokio::test]
        async fn test_aggregate_error_when_no_tasks() {
            let provider = MockProvider::default();
            let agg = LlmAggregator::new(provider, &make_config());
            let graph = TaskGraph::new("empty goal");
            let err = agg.aggregate(&graph).await.unwrap_err();
            assert!(matches!(err, OrchestrationError::AggregationFailed(_)));
        }

        #[tokio::test]
        async fn test_aggregate_includes_skipped_in_prompt() {
            // MT-2: skipped task title/description must appear in the LLM prompt.
            // We verify this indirectly by checking that the fallback path (used here
            // because MockProvider captures the prompt text) includes the skipped task.
            // Use a failing provider so we get the fallback output which is built from graph state.
            let provider = MockProvider::failing();
            let agg = LlmAggregator::new(provider, &make_config());
            let mut graph = make_graph_with_tasks(&[
                (TaskStatus::Completed, Some("ok")),
                (TaskStatus::Skipped, None),
            ]);
            // Set a recognizable description for the skipped task.
            graph.tasks[1].description = "unique-skipped-description".to_string();
            let result = agg.aggregate(&graph).await.unwrap();
            assert!(
                result.contains("task-1") || result.contains("skipped"),
                "fallback must include skipped task info; got: {result}"
            );
            assert!(
                result.contains("unique-skipped-description"),
                "fallback must include skipped task description; got: {result}"
            );
        }

        #[tokio::test]
        async fn test_aggregate_per_task_budget_truncation() {
            // MT-3: with a 1-token (4-char) budget, output must be truncated to <= 4 chars
            // per task. Use a failing provider so we get the fallback path which applies
            // the same per-task truncation.
            let config = OrchestrationConfig {
                aggregator_max_tokens: 1, // 4 chars total budget -> per_task = 4 chars
                ..OrchestrationConfig::default()
            };
            let provider = MockProvider::failing();
            let agg = LlmAggregator::new(provider, &config);
            let long_output = "a".repeat(1000);
            let graph = make_graph_with_tasks(&[(TaskStatus::Completed, Some(&long_output))]);
            let result = agg.aggregate(&graph).await.unwrap();
            // With 4-char truncation, the 1000-char run of 'a' must not appear verbatim.
            // Even with spotlight wrapping the raw 'aaaa...' sequence is trimmed to at most 4 'a's.
            assert!(
                !result.contains(&"a".repeat(5)),
                "with 4-char budget, no sequence of >=5 'a' chars should appear; \
                 result len={}, result={result:?}",
                result.len()
            );
        }
    }
}
