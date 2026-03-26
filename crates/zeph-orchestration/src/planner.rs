// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! LLM-based goal decomposition into a validated `TaskGraph`.

use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use zeph_llm::provider::{LlmProvider, Message, Role};

use super::dag;
use super::error::OrchestrationError;
use super::graph::{ExecutionMode, FailureStrategy, TaskGraph, TaskId, TaskNode};
use zeph_config::OrchestrationConfig;
use zeph_subagent::{SubAgentDef, ToolPolicy};

/// Decomposes a high-level goal into a validated `TaskGraph`.
#[allow(async_fn_in_trait)]
pub trait Planner: Send + Sync {
    /// Generate a task graph from a user goal.
    ///
    /// `available_agents` provides the set of agent definitions the planner
    /// can reference in `agent_hint` fields. Unknown hints produce a warning
    /// but do not fail planning.
    ///
    /// Returns the task graph and the LLM token usage `(prompt, completion)` from the
    /// underlying API call, or `None` when the provider does not report usage.
    ///
    /// # Errors
    ///
    /// Returns `OrchestrationError::PlanningFailed` if the LLM response cannot
    /// be parsed into a valid graph after retry, or if DAG validation fails.
    async fn plan(
        &self,
        goal: &str,
        available_agents: &[SubAgentDef],
    ) -> Result<(TaskGraph, Option<(u64, u64)>), OrchestrationError>;
}

/// LLM-backed `Planner` using `chat_typed` for structured JSON output.
pub struct LlmPlanner<P: LlmProvider> {
    provider: P,
    max_tasks: u32,
}

impl<P: LlmProvider> LlmPlanner<P> {
    /// Create a new `LlmPlanner` from a provider and config.
    ///
    /// `config.planner_model` is reserved for future caller-side provider selection;
    /// `LlmPlanner` uses whatever provider it receives.
    #[must_use]
    pub fn new(provider: P, config: &OrchestrationConfig) -> Self {
        Self {
            provider,
            max_tasks: config.max_tasks,
        }
    }
}

/// JSON schema for the LLM planner response. Internal parsing type.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub(crate) struct PlannerResponse {
    pub tasks: Vec<PlannedTask>,
}

/// A single task in the raw LLM planner response. Internal parsing type.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub(crate) struct PlannedTask {
    pub task_id: String,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub agent_hint: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub failure_strategy: Option<String>,
    /// LLM-annotated execution mode. Absent or `null` defaults to `Parallel`.
    #[serde(default)]
    pub execution_mode: Option<ExecutionMode>,
}

impl<P: LlmProvider + Send + Sync> Planner for LlmPlanner<P> {
    async fn plan(
        &self,
        goal: &str,
        available_agents: &[SubAgentDef],
    ) -> Result<(TaskGraph, Option<(u64, u64)>), OrchestrationError> {
        if goal.trim().is_empty() {
            return Err(OrchestrationError::PlanningFailed(
                "goal cannot be empty".into(),
            ));
        }

        let messages = build_prompt(goal, available_agents, self.max_tasks);

        let response: PlannerResponse = self
            .provider
            .chat_typed(&messages)
            .await
            .map_err(|e| OrchestrationError::PlanningFailed(e.to_string()))?;

        // Capture usage right after the API call, before any fallible post-processing.
        let usage = self.provider.last_usage();

        let graph = convert_response(response, goal, available_agents, self.max_tasks)?;

        dag::validate(&graph.tasks, self.max_tasks as usize)?;

        Ok((graph, usage))
    }
}

/// Build the prompt messages for the planner LLM call.
fn build_prompt(goal: &str, agents: &[SubAgentDef], max_tasks: u32) -> Vec<Message> {
    let agent_catalog = agents
        .iter()
        .map(|a| {
            let tools = match &a.tools {
                ToolPolicy::AllowList(list) => list.join(", "),
                ToolPolicy::DenyList(excluded) => {
                    format!("all except: [{}]", excluded.join(", "))
                }
                ToolPolicy::InheritAll => "all".to_string(),
            };
            format!(
                "- name: \"{}\", description: \"{}\", tools: [{}]",
                a.name, a.description, tools
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let system = format!(
        "You are a task planner. Decompose the user's goal into \
         independent sub-tasks that can be executed by the available agents.\n\n\
         Available agents:\n{agent_catalog}\n\n\
         Rules:\n\
         - Each task must have a unique task_id (short, descriptive, kebab-case: [a-z0-9-]).\n\
         - Each task must have a clear, actionable title and description.\n\
         - The description should be a complete prompt for the assigned agent.\n\
         - Specify dependencies using task_id strings in depends_on.\n\
         - Maximize parallelism: only add a dependency when the output is truly needed.\n\
         - Do not create more than {max_tasks} tasks.\n\
         - Assign agent_hint when a specific agent is clearly appropriate.\n\
         - failure_strategy is optional: \"abort\", \"retry\", \"skip\", \"ask\", or omit for default.\n\
         - For each task, specify execution_mode: \"parallel\" (can run concurrently with sibling \
           tasks) or \"sequential\" (must run alone at its DAG level, e.g. deploy, shared-state \
           mutation, exclusive resource access). Default to \"parallel\" when unsure.\n\n\
         Example (2-task plan):\n\
         {{\"tasks\": [\
           {{\"task_id\": \"fetch-data\", \"title\": \"Fetch raw data\", \
             \"description\": \"Download the dataset from source.\", \
             \"depends_on\": [], \"execution_mode\": \"parallel\"}},\
           {{\"task_id\": \"deploy\", \"title\": \"Deploy service\", \
             \"description\": \"Deploy the processed artifact to production.\", \
             \"depends_on\": [\"fetch-data\"], \"execution_mode\": \"sequential\"}}\
         ]}}"
    );

    // The goal is typed directly by the user and is trusted input.
    let user = format!("Decompose this goal into tasks:\n\n{goal}");

    vec![
        Message::from_legacy(Role::System, system),
        Message::from_legacy(Role::User, user),
    ]
}

/// Validate that a `task_id` conforms to kebab-case: `^[a-z0-9]([a-z0-9-]*[a-z0-9])?$`
/// Single-char IDs (`[a-z0-9]`) are also valid. Maximum length is 64 characters.
fn is_valid_task_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 64 {
        return false;
    }
    let bytes = id.as_bytes();
    // Must start and end with [a-z0-9]
    let first_ok = bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit();
    let last_ok =
        bytes[bytes.len() - 1].is_ascii_lowercase() || bytes[bytes.len() - 1].is_ascii_digit();
    if !first_ok || !last_ok {
        return false;
    }
    // All chars must be [a-z0-9-]
    bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// Convert a raw `PlannerResponse` into a `TaskGraph`.
///
/// Maps string `task_id` values to `TaskId(u32)` via a position-based `HashMap`.
/// Public within the crate for use by `plan_cache::adapt_plan`.
pub(crate) fn convert_response_pub(
    response: PlannerResponse,
    goal: &str,
    available_agents: &[SubAgentDef],
    max_tasks: u32,
) -> Result<TaskGraph, OrchestrationError> {
    convert_response(response, goal, available_agents, max_tasks)
}

fn convert_response(
    response: PlannerResponse,
    goal: &str,
    available_agents: &[SubAgentDef],
    max_tasks: u32,
) -> Result<TaskGraph, OrchestrationError> {
    let planned = response.tasks;

    if planned.is_empty() {
        return Err(OrchestrationError::PlanningFailed(
            "planner returned zero tasks".into(),
        ));
    }
    if planned.len() > max_tasks as usize {
        return Err(OrchestrationError::PlanningFailed(format!(
            "planner returned {} tasks, exceeding limit of {max_tasks}",
            planned.len()
        )));
    }

    // Validate task_id format and build string -> index map
    for pt in &planned {
        if !is_valid_task_id(&pt.task_id) {
            return Err(OrchestrationError::PlanningFailed(format!(
                "invalid task_id '{}': must match ^[a-z0-9]([a-z0-9-]*[a-z0-9])?$",
                pt.task_id
            )));
        }
    }

    let id_map: HashMap<&str, u32> = planned
        .iter()
        .enumerate()
        .map(|(i, t)| {
            u32::try_from(i)
                .map(|idx| (t.task_id.as_str(), idx))
                .map_err(|_| {
                    OrchestrationError::PlanningFailed(format!("task index {i} overflows u32"))
                })
        })
        .collect::<Result<_, _>>()?;

    // Check for duplicate task_ids
    if id_map.len() != planned.len() {
        return Err(OrchestrationError::PlanningFailed(
            "duplicate task_id in planner output".into(),
        ));
    }

    let agent_names: HashSet<&str> = available_agents.iter().map(|a| a.name.as_str()).collect();

    let mut graph = TaskGraph::new(goal);

    for (i, pt) in planned.iter().enumerate() {
        let idx = u32::try_from(i).map_err(|_| {
            OrchestrationError::PlanningFailed(format!("task index {i} overflows u32"))
        })?;
        let mut node = TaskNode::new(idx, &pt.title, &pt.description);

        for dep_str in &pt.depends_on {
            match id_map.get(dep_str.as_str()) {
                Some(&dep_idx) => node.depends_on.push(TaskId(dep_idx)),
                None => {
                    return Err(OrchestrationError::PlanningFailed(format!(
                        "task '{}' depends on unknown task_id '{dep_str}'",
                        pt.task_id
                    )));
                }
            }
        }

        if let Some(hint) = &pt.agent_hint {
            if agent_names.contains(hint.as_str()) {
                node.agent_hint = Some(hint.clone());
            } else {
                tracing::warn!(
                    task_id = %pt.task_id,
                    agent_hint = %hint,
                    "unknown agent_hint in planner output, ignoring"
                );
            }
        }

        if let Some(fs_str) = &pt.failure_strategy {
            match fs_str.parse::<FailureStrategy>() {
                Ok(fs) => node.failure_strategy = Some(fs),
                Err(_) => {
                    tracing::warn!(
                        task_id = %pt.task_id,
                        strategy = %fs_str,
                        "invalid failure_strategy in planner output, using default"
                    );
                }
            }
        }

        if let Some(mode) = pt.execution_mode {
            node.execution_mode = mode;
        }

        graph.tasks.push(node);
    }

    Ok(graph)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::needless_pass_by_value)]

    use super::*;
    use zeph_subagent::{SkillFilter, SubAgentDef, SubAgentPermissions, SubagentHooks, ToolPolicy};

    fn make_agent(name: &str, tools: ToolPolicy) -> SubAgentDef {
        SubAgentDef {
            name: name.to_string(),
            description: format!("{name} agent"),
            model: None,
            tools,
            disallowed_tools: Vec::new(),
            permissions: SubAgentPermissions::default(),
            skills: SkillFilter::default(),
            system_prompt: String::new(),
            hooks: SubagentHooks::default(),
            memory: None,
            source: None,
            file_path: None,
        }
    }

    fn make_planned(
        task_id: &str,
        title: &str,
        deps: &[&str],
        agent_hint: Option<&str>,
    ) -> PlannedTask {
        PlannedTask {
            task_id: task_id.to_string(),
            title: title.to_string(),
            description: format!("do {title}"),
            agent_hint: agent_hint.map(std::string::ToString::to_string),
            depends_on: deps.iter().map(std::string::ToString::to_string).collect(),
            failure_strategy: None,
            execution_mode: None,
        }
    }

    fn agents() -> Vec<SubAgentDef> {
        vec![
            make_agent("agent-a", ToolPolicy::InheritAll),
            make_agent("agent-b", ToolPolicy::AllowList(vec!["shell".to_string()])),
        ]
    }

    // --- convert_response tests ---

    #[test]
    fn test_convert_valid_linear_chain() {
        let response = PlannerResponse {
            tasks: vec![
                make_planned("task-a", "Task A", &[], None),
                make_planned("task-b", "Task B", &["task-a"], None),
                make_planned("task-c", "Task C", &["task-b"], None),
            ],
        };
        let graph = convert_response(response, "linear goal", &agents(), 20).unwrap();
        assert_eq!(graph.tasks.len(), 3);
        assert_eq!(graph.tasks[0].id, TaskId(0));
        assert_eq!(graph.tasks[1].depends_on, vec![TaskId(0)]);
        assert_eq!(graph.tasks[2].depends_on, vec![TaskId(1)]);
    }

    #[test]
    fn test_convert_valid_diamond() {
        // A -> B, A -> C, B+C -> D
        let response = PlannerResponse {
            tasks: vec![
                make_planned("a", "A", &[], None),
                make_planned("b", "B", &["a"], None),
                make_planned("c", "C", &["a"], None),
                make_planned("d", "D", &["b", "c"], None),
            ],
        };
        let graph = convert_response(response, "diamond", &agents(), 20).unwrap();
        assert_eq!(graph.tasks[3].depends_on, vec![TaskId(1), TaskId(2)]);
    }

    #[test]
    fn test_convert_parallel_tasks() {
        let response = PlannerResponse {
            tasks: vec![
                make_planned("t1", "T1", &[], None),
                make_planned("t2", "T2", &[], None),
                make_planned("t3", "T3", &[], None),
            ],
        };
        let graph = convert_response(response, "parallel", &agents(), 20).unwrap();
        for node in &graph.tasks {
            assert!(node.depends_on.is_empty());
        }
    }

    #[test]
    fn test_convert_empty_tasks_rejected() {
        let response = PlannerResponse { tasks: vec![] };
        let err = convert_response(response, "goal", &agents(), 20).unwrap_err();
        assert!(matches!(err, OrchestrationError::PlanningFailed(_)));
    }

    #[test]
    fn test_convert_exceeds_max_tasks() {
        let tasks = (0..5)
            .map(|i| make_planned(&format!("task-{i}"), &format!("T{i}"), &[], None))
            .collect();
        let response = PlannerResponse { tasks };
        let err = convert_response(response, "goal", &agents(), 3).unwrap_err();
        assert!(matches!(err, OrchestrationError::PlanningFailed(_)));
    }

    #[test]
    fn test_convert_duplicate_task_ids() {
        // Two tasks with same id — duplicate check should catch this
        // Note: HashMap deduplicates by key, so id_map.len() < planned.len()
        let response = PlannerResponse {
            tasks: vec![
                make_planned("dup", "First", &[], None),
                make_planned("dup", "Second", &[], None),
            ],
        };
        let err = convert_response(response, "goal", &agents(), 20).unwrap_err();
        assert!(matches!(err, OrchestrationError::PlanningFailed(_)));
    }

    #[test]
    fn test_convert_unknown_dependency() {
        let response = PlannerResponse {
            tasks: vec![make_planned("task-a", "A", &["nonexistent"], None)],
        };
        let err = convert_response(response, "goal", &agents(), 20).unwrap_err();
        assert!(matches!(err, OrchestrationError::PlanningFailed(_)));
    }

    #[test]
    fn test_convert_unknown_agent_hint_warns() {
        let response = PlannerResponse {
            tasks: vec![make_planned("task-a", "A", &[], Some("unknown-agent"))],
        };
        let graph = convert_response(response, "goal", &agents(), 20).unwrap();
        assert!(graph.tasks[0].agent_hint.is_none());
    }

    #[test]
    fn test_convert_known_agent_hint_stored() {
        let response = PlannerResponse {
            tasks: vec![make_planned("task-a", "A", &[], Some("agent-a"))],
        };
        let graph = convert_response(response, "goal", &agents(), 20).unwrap();
        assert_eq!(graph.tasks[0].agent_hint.as_deref(), Some("agent-a"));
    }

    #[test]
    fn test_convert_invalid_task_id_format() {
        let cases = vec![
            "",         // empty
            " ",        // whitespace only
            "Task A",   // contains space
            "-task",    // starts with dash
            "task-",    // ends with dash
            "TASK",     // uppercase
            "task_one", // underscore
            "задача",   // non-ASCII
        ];
        for bad_id in cases {
            let response = PlannerResponse {
                tasks: vec![PlannedTask {
                    task_id: bad_id.to_string(),
                    title: "T".to_string(),
                    description: "d".to_string(),
                    agent_hint: None,
                    depends_on: vec![],
                    failure_strategy: None,
                    execution_mode: None,
                }],
            };
            let err = convert_response(response, "goal", &agents(), 20).unwrap_err();
            assert!(
                matches!(err, OrchestrationError::PlanningFailed(_)),
                "expected PlanningFailed for task_id '{bad_id}'"
            );
        }
    }

    #[test]
    fn test_convert_valid_task_id_formats() {
        let cases = vec!["a", "a1", "task-a", "fetch-data-v2", "0"];
        for id in cases {
            assert!(is_valid_task_id(id), "expected valid: '{id}'");
        }
    }

    #[test]
    fn test_convert_invalid_failure_strategy_uses_none() {
        let response = PlannerResponse {
            tasks: vec![PlannedTask {
                task_id: "task-a".to_string(),
                title: "A".to_string(),
                description: "d".to_string(),
                agent_hint: None,
                depends_on: vec![],
                failure_strategy: Some("explode".to_string()),
                execution_mode: None,
            }],
        };
        let graph = convert_response(response, "goal", &agents(), 20).unwrap();
        assert!(graph.tasks[0].failure_strategy.is_none());
    }

    #[test]
    fn test_convert_goal_is_set() {
        let response = PlannerResponse {
            tasks: vec![make_planned("t1", "T1", &[], None)],
        };
        let graph = convert_response(response, "my goal", &agents(), 20).unwrap();
        assert_eq!(graph.goal, "my goal");
    }

    // --- build_prompt tests ---

    #[test]
    fn test_build_prompt_includes_agent_catalog() {
        let msgs = build_prompt("do something", &agents(), 20);
        let text = &msgs[0].content;
        assert!(text.contains("agent-a"));
        assert!(text.contains("agent-b"));
        assert!(text.contains("shell"));
    }

    #[test]
    fn test_build_prompt_includes_max_tasks() {
        let msgs = build_prompt("goal", &agents(), 42);
        let text = &msgs[0].content;
        assert!(text.contains("42"));
    }

    #[test]
    fn test_build_prompt_deny_list_renders_as_except() {
        let a = make_agent(
            "restricted",
            ToolPolicy::DenyList(vec!["shell".to_string(), "web".to_string()]),
        );
        let msgs = build_prompt("goal", &[a], 20);
        let text = &msgs[0].content;
        assert!(text.contains("all except:"));
        assert!(text.contains("shell"));
        assert!(text.contains("web"));
    }

    #[test]
    fn test_build_prompt_has_two_messages() {
        let msgs = build_prompt("goal", &agents(), 20);
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn test_build_prompt_includes_example_json() {
        let msgs = build_prompt("goal", &agents(), 20);
        let text = &msgs[0].content;
        assert!(
            text.contains("fetch-data"),
            "example should include fetch-data task_id"
        );
        assert!(
            text.contains("depends_on"),
            "example should show depends_on field"
        );
    }

    // --- execution_mode tests ---

    #[test]
    fn convert_execution_mode_parallel() {
        let response = PlannerResponse {
            tasks: vec![PlannedTask {
                task_id: "t1".to_string(),
                title: "T1".to_string(),
                description: "d".to_string(),
                agent_hint: None,
                depends_on: vec![],
                failure_strategy: None,
                execution_mode: Some(ExecutionMode::Parallel),
            }],
        };
        let graph = convert_response(response, "goal", &agents(), 20).unwrap();
        assert_eq!(graph.tasks[0].execution_mode, ExecutionMode::Parallel);
    }

    #[test]
    fn convert_execution_mode_sequential() {
        let response = PlannerResponse {
            tasks: vec![PlannedTask {
                task_id: "t1".to_string(),
                title: "T1".to_string(),
                description: "d".to_string(),
                agent_hint: None,
                depends_on: vec![],
                failure_strategy: None,
                execution_mode: Some(ExecutionMode::Sequential),
            }],
        };
        let graph = convert_response(response, "goal", &agents(), 20).unwrap();
        assert_eq!(graph.tasks[0].execution_mode, ExecutionMode::Sequential);
    }

    #[test]
    fn convert_execution_mode_missing_defaults_parallel() {
        let response = PlannerResponse {
            tasks: vec![make_planned("t1", "T1", &[], None)],
        };
        let graph = convert_response(response, "goal", &agents(), 20).unwrap();
        assert_eq!(graph.tasks[0].execution_mode, ExecutionMode::Parallel);
    }

    #[test]
    fn build_prompt_includes_execution_mode() {
        let msgs = build_prompt("goal", &agents(), 20);
        let text = &msgs[0].content;
        assert!(
            text.contains("execution_mode"),
            "prompt must mention execution_mode field"
        );
        assert!(
            text.contains("sequential"),
            "prompt must mention sequential option"
        );
    }

    // --- plan() integration tests ---

    mod integration {
        use super::*;
        use zeph_llm::mock::MockProvider;

        fn valid_json_response() -> String {
            r#"{"tasks": [
                {"task_id": "step-one", "title": "Step one", "description": "Do step one", "depends_on": []},
                {"task_id": "step-two", "title": "Step two", "description": "Do step two", "depends_on": ["step-one"]}
            ]}"#
            .to_string()
        }

        fn cyclic_json_response() -> String {
            r#"{"tasks": [
                {"task_id": "a", "title": "A", "description": "A desc", "depends_on": ["b"]},
                {"task_id": "b", "title": "B", "description": "B desc", "depends_on": ["a"]}
            ]}"#
            .to_string()
        }

        fn single_task_json() -> String {
            r#"{"tasks": [
                {"task_id": "only-task", "title": "The task", "description": "Do it", "depends_on": []}
            ]}"#
            .to_string()
        }

        fn make_config() -> OrchestrationConfig {
            OrchestrationConfig::default()
        }

        #[tokio::test]
        async fn test_plan_valid_response() {
            let provider = MockProvider::with_responses(vec![valid_json_response()]);
            let planner = LlmPlanner::new(provider, &make_config());
            let (graph, _usage) = planner.plan("build and deploy", &agents()).await.unwrap();
            assert_eq!(graph.tasks.len(), 2);
            assert_eq!(graph.goal, "build and deploy");
        }

        #[tokio::test]
        async fn test_plan_cycle_rejected() {
            // A single response suffices: cyclic_json_response() is valid JSON, so chat_typed
            // parses it successfully on the first attempt without triggering a retry.
            // The cycle is caught by dag::validate after convert_response succeeds.
            let provider = MockProvider::with_responses(vec![cyclic_json_response()]);
            let planner = LlmPlanner::new(provider, &make_config());
            let err = planner.plan("cyclic", &agents()).await.unwrap_err();
            assert!(matches!(err, OrchestrationError::CycleDetected));
        }

        #[tokio::test]
        async fn test_plan_empty_goal_rejected() {
            let provider = MockProvider::default();
            let planner = LlmPlanner::new(provider, &make_config());
            let err = planner.plan("   ", &agents()).await.unwrap_err();
            assert!(matches!(err, OrchestrationError::PlanningFailed(_)));
        }

        #[tokio::test]
        async fn test_plan_llm_error_maps_to_planning_failed() {
            let provider = MockProvider::failing();
            let planner = LlmPlanner::new(provider, &make_config());
            let err = planner.plan("valid goal", &agents()).await.unwrap_err();
            assert!(matches!(err, OrchestrationError::PlanningFailed(_)));
        }

        #[tokio::test]
        async fn test_plan_invalid_failure_strategy_warns() {
            let json = r#"{"tasks": [
                {"task_id": "t1", "title": "T1", "description": "d", "depends_on": [],
                 "failure_strategy": "explode"}
            ]}"#
            .to_string();
            let provider = MockProvider::with_responses(vec![json]);
            let planner = LlmPlanner::new(provider, &make_config());
            let (graph, _usage) = planner.plan("goal", &agents()).await.unwrap();
            assert!(graph.tasks[0].failure_strategy.is_none());
        }

        #[tokio::test]
        async fn test_plan_single_task_goal() {
            let provider = MockProvider::with_responses(vec![single_task_json()]);
            let planner = LlmPlanner::new(provider, &make_config());
            let (graph, _usage) = planner.plan("simple task", &agents()).await.unwrap();
            assert_eq!(graph.tasks.len(), 1);
            assert!(graph.tasks[0].depends_on.is_empty());
        }

        #[tokio::test]
        async fn test_plan_execution_mode_from_json() {
            let json = r#"{"tasks": [
                {"task_id": "t1", "title": "T1", "description": "d", "depends_on": [],
                 "execution_mode": "parallel"},
                {"task_id": "t2", "title": "T2", "description": "d", "depends_on": ["t1"],
                 "execution_mode": "sequential"}
            ]}"#
            .to_string();
            let provider = MockProvider::with_responses(vec![json]);
            let planner = LlmPlanner::new(provider, &make_config());
            let (graph, _usage) = planner.plan("goal", &agents()).await.unwrap();
            assert_eq!(graph.tasks[0].execution_mode, ExecutionMode::Parallel);
            assert_eq!(graph.tasks[1].execution_mode, ExecutionMode::Sequential);
        }

        #[tokio::test]
        async fn test_plan_execution_mode_null_defaults_parallel() {
            let json = r#"{"tasks": [
                {"task_id": "t1", "title": "T1", "description": "d", "depends_on": [],
                 "execution_mode": null}
            ]}"#
            .to_string();
            let provider = MockProvider::with_responses(vec![json]);
            let planner = LlmPlanner::new(provider, &make_config());
            let (graph, _usage) = planner.plan("goal", &agents()).await.unwrap();
            assert_eq!(graph.tasks[0].execution_mode, ExecutionMode::Parallel);
        }
    }
}
