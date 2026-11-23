// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Acceptance tests for #7158: the rule-based router compares tool entries by runtime id.

use zeph_orchestration::{
    AgentRouter, ExecutionMode, RuleBasedRouter, TaskId, TaskNode, TaskStatus,
};
use zeph_subagent::{SubAgentDef, ToolPolicy};

fn task(desc: &str) -> TaskNode {
    TaskNode {
        id: TaskId(0),
        title: "t".to_owned(),
        description: desc.to_owned(),
        agent_hint: None,
        status: TaskStatus::Pending,
        depends_on: vec![],
        result: None,
        assigned_agent: None,
        retry_count: 0,
        predicate_rerun_count: 0,
        failure_strategy: None,
        max_retries: None,
        execution_mode: ExecutionMode::default(),
        verify_predicate: None,
        predicate_outcome: None,
        execution_environment: None,
        token_budget_cents: None,
        network_scope: None,
        tool_allowlist: None,
        asset_sensitivity: None,
        timeout: None,
        recovery: None,
        routed_from: None,
        commanded_from: None,
        handoff_rejected: None,
    }
}

fn def(name: &str, tools: ToolPolicy) -> SubAgentDef {
    let mut d = SubAgentDef::for_test(name);
    d.tools = tools;
    d
}

#[test]
fn test_route_allow_list_with_runtime_ids_selects_matching_agent() {
    let defs = vec![
        def("writer", ToolPolicy::AllowList(vec!["write".into()])),
        def("reader", ToolPolicy::AllowList(vec!["read".into()])),
    ];
    let picked = RuleBasedRouter.route(&task("analyze the module"), &defs);
    assert_eq!(picked.as_deref(), Some("reader"));
}

#[test]
fn test_route_except_runtime_id_excludes_agent() {
    let mut no_shell = def("no-shell", ToolPolicy::InheritAll);
    no_shell.disallowed_tools = vec!["bash".into()];
    let defs = vec![no_shell, def("shell-ok", ToolPolicy::InheritAll)];
    let picked = RuleBasedRouter.route(&task("build the project"), &defs);
    assert_eq!(picked.as_deref(), Some("shell-ok"));
}

#[test]
fn test_route_shell_alias_in_allow_list_counts_as_bash() {
    let defs = vec![
        def("reader", ToolPolicy::AllowList(vec!["read".into()])),
        def("shell-user", ToolPolicy::AllowList(vec!["shell".into()])),
    ];
    let picked = RuleBasedRouter.route(&task("run test suite and compile"), &defs);
    assert_eq!(picked.as_deref(), Some("shell-user"));
}
