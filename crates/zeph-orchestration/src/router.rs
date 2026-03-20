// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Agent routing: selects the best agent definition for a given task.

use zeph_subagent::{SubAgentDef, ToolPolicy};

use super::graph::TaskNode;

/// Selects the best agent definition for a given task.
pub trait AgentRouter: Send + Sync {
    /// Choose an agent for the task.
    ///
    /// Returns the agent definition name, or `None` if no suitable agent was found.
    fn route(&self, task: &TaskNode, available: &[SubAgentDef]) -> Option<String>;
}

/// Rule-based agent router with a 3-step fallback chain:
///
/// 1. `task.agent_hint` exact match against available agent names.
/// 2. Tool requirement matching: keywords in task description matched against agent
///    tool policies (last-resort heuristic — see limitations note).
/// 3. First available agent (fallback).
///
/// # Limitations
///
/// The keyword-to-tool matching (step 2) is intentionally basic. Common English words
/// ("read", "build", "review", "edit") frequently appear in task descriptions unrelated
/// to specific tool requirements. For reliable routing, the planner should always set
/// `task.agent_hint` explicitly. Step 2 is a fallback for when no hint is provided and
/// no exact match is found — treat it as a best-effort heuristic, not authoritative routing.
/// The ultimate fallback (step 3) returns the first available agent unconditionally.
///
/// Step 2 only matches English keywords. Non-English task descriptions will always fall
/// through to step 3.
pub struct RuleBasedRouter;

/// Keyword-to-tool mapping for step 2 routing.
///
/// Maps a lowercase substring of the task description to a tool name.
/// Only matched when `agent_hint` is absent or not found (step 2 is last resort).
const TOOL_KEYWORDS: &[(&str, &str)] = &[
    ("write code", "Write"),
    ("implement", "Write"),
    ("create file", "Write"),
    ("edit", "Edit"),
    ("modify", "Edit"),
    ("read", "Read"),
    ("analyze", "Read"),
    ("review", "Read"),
    ("run test", "Bash"),
    ("execute", "Bash"),
    ("compile", "Bash"),
    ("build", "Bash"),
    ("search", "Grep"),
];

impl AgentRouter for RuleBasedRouter {
    fn route(&self, task: &TaskNode, available: &[SubAgentDef]) -> Option<String> {
        if available.is_empty() {
            return None;
        }

        // Step 1: exact match on agent_hint.
        if let Some(ref hint) = task.agent_hint {
            if available.iter().any(|d| d.name == *hint) {
                return Some(hint.clone());
            }
            tracing::debug!(
                task_id = %task.id,
                hint = %hint,
                "agent_hint not found in available agents, falling back to tool matching"
            );
        }

        // Step 2: tool requirement matching by keyword scoring.
        let desc_lower = task.description.to_lowercase();
        let mut best_match: Option<(&SubAgentDef, usize)> = None;

        for def in available {
            let score = TOOL_KEYWORDS
                .iter()
                .filter(|(keyword, tool_name)| {
                    desc_lower.contains(*keyword) && agent_has_tool(def, tool_name)
                })
                .count();

            if score > 0 && best_match.as_ref().is_none_or(|(_, best)| score > *best) {
                best_match = Some((def, score));
            }
        }

        if let Some((def, _)) = best_match {
            return Some(def.name.clone());
        }

        // Step 3: first available agent (unconditional fallback).
        Some(available[0].name.clone())
    }
}

/// Check if an agent definition allows a specific tool.
fn agent_has_tool(def: &SubAgentDef, tool_name: &str) -> bool {
    // Explicit deny-list wins over everything.
    if def.disallowed_tools.iter().any(|t| t == tool_name) {
        return false;
    }

    match &def.tools {
        ToolPolicy::AllowList(allowed) => allowed.iter().any(|t| t == tool_name),
        ToolPolicy::DenyList(denied) => !denied.iter().any(|t| t == tool_name),
        ToolPolicy::InheritAll => true,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::default_trait_access)]

    use super::*;
    use crate::graph::{TaskId, TaskNode, TaskStatus};
    use zeph_subagent::{SkillFilter, SubAgentPermissions, ToolPolicy};

    fn make_task(id: u32, desc: &str, hint: Option<&str>) -> TaskNode {
        TaskNode {
            id: TaskId(id),
            title: format!("task-{id}"),
            description: desc.to_string(),
            agent_hint: hint.map(str::to_string),
            status: TaskStatus::Pending,
            depends_on: vec![],
            result: None,
            assigned_agent: None,
            retry_count: 0,
            failure_strategy: None,
            max_retries: None,
            handoff_context: None,
        }
    }

    fn make_def(name: &str, tools: ToolPolicy) -> SubAgentDef {
        SubAgentDef {
            name: name.to_string(),
            description: format!("{name} agent"),
            model: None,
            tools,
            disallowed_tools: vec![],
            permissions: SubAgentPermissions::default(),
            skills: SkillFilter::default(),
            system_prompt: String::new(),
            hooks: zeph_subagent::SubagentHooks::default(),
            memory: None,
            source: None,
            file_path: None,
        }
    }

    fn make_def_with_disallowed(
        name: &str,
        tools: ToolPolicy,
        disallowed: Vec<String>,
    ) -> SubAgentDef {
        SubAgentDef {
            name: name.to_string(),
            description: format!("{name} agent"),
            model: None,
            tools,
            disallowed_tools: disallowed,
            permissions: SubAgentPermissions::default(),
            skills: SkillFilter::default(),
            system_prompt: String::new(),
            hooks: zeph_subagent::SubagentHooks::default(),
            memory: None,
            source: None,
            file_path: None,
        }
    }

    // --- AgentRouter tests ---

    #[test]
    fn test_route_agent_hint_match() {
        let task = make_task(0, "do something", Some("specialist"));
        let available = vec![
            make_def("generalist", ToolPolicy::InheritAll),
            make_def("specialist", ToolPolicy::InheritAll),
        ];
        let router = RuleBasedRouter;
        let result = router.route(&task, &available);
        assert_eq!(result.as_deref(), Some("specialist"));
    }

    #[test]
    fn test_route_agent_hint_not_found_fallback() {
        let task = make_task(0, "do something simple", Some("missing-agent"));
        let available = vec![make_def("worker", ToolPolicy::InheritAll)];
        let router = RuleBasedRouter;
        // hint not found → falls back to first available
        let result = router.route(&task, &available);
        assert_eq!(result.as_deref(), Some("worker"));
    }

    #[test]
    fn test_route_tool_matching() {
        let task = make_task(0, "implement the new feature by writing code", None);
        let available = vec![
            make_def(
                "readonly-agent",
                ToolPolicy::AllowList(vec!["Read".to_string()]),
            ),
            make_def(
                "writer-agent",
                ToolPolicy::AllowList(vec!["Write".to_string(), "Edit".to_string()]),
            ),
        ];
        let router = RuleBasedRouter;
        let result = router.route(&task, &available);
        // "implement" and "write code" match Write tool → writer-agent
        assert_eq!(result.as_deref(), Some("writer-agent"));
    }

    #[test]
    fn test_route_fallback_first_available() {
        // No hint, no keyword matches → first available.
        let task = make_task(0, "xyz123 abstract task", None);
        let available = vec![
            make_def("alpha", ToolPolicy::InheritAll),
            make_def("beta", ToolPolicy::InheritAll),
        ];
        let router = RuleBasedRouter;
        let result = router.route(&task, &available);
        assert_eq!(result.as_deref(), Some("alpha"));
    }

    #[test]
    fn test_route_empty_returns_none() {
        let task = make_task(0, "do something", None);
        let router = RuleBasedRouter;
        let result = router.route(&task, &[]);
        assert!(result.is_none());
    }

    // --- agent_has_tool tests ---

    #[test]
    fn test_agent_has_tool_allow_list() {
        let def = make_def(
            "a",
            ToolPolicy::AllowList(vec!["Read".to_string(), "Write".to_string()]),
        );
        assert!(agent_has_tool(&def, "Read"));
        assert!(agent_has_tool(&def, "Write"));
        assert!(!agent_has_tool(&def, "Bash"));
    }

    #[test]
    fn test_agent_has_tool_deny_list() {
        let def = make_def("a", ToolPolicy::DenyList(vec!["Bash".to_string()]));
        assert!(agent_has_tool(&def, "Read"));
        assert!(agent_has_tool(&def, "Write"));
        assert!(!agent_has_tool(&def, "Bash"));
    }

    #[test]
    fn test_agent_has_tool_inherit_all() {
        let def = make_def("a", ToolPolicy::InheritAll);
        assert!(agent_has_tool(&def, "Read"));
        assert!(agent_has_tool(&def, "Bash"));
        assert!(agent_has_tool(&def, "AnythingGoes"));
    }

    #[test]
    fn test_agent_has_tool_disallowed_wins_over_allow_list() {
        // disallowed_tools takes priority even when tool is in AllowList.
        let def = make_def_with_disallowed(
            "a",
            ToolPolicy::AllowList(vec!["Read".to_string(), "Bash".to_string()]),
            vec!["Bash".to_string()],
        );
        assert!(agent_has_tool(&def, "Read"));
        assert!(!agent_has_tool(&def, "Bash"), "disallowed_tools must win");
    }

    #[test]
    fn test_agent_has_tool_disallowed_wins_over_inherit_all() {
        let def = make_def_with_disallowed(
            "a",
            ToolPolicy::InheritAll,
            vec!["DangerousTool".to_string()],
        );
        assert!(agent_has_tool(&def, "Read"));
        assert!(!agent_has_tool(&def, "DangerousTool"));
    }
}
