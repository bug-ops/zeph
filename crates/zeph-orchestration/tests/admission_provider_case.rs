// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Acceptance tests for #7179: the admission gate keys a sub-agent's provider by the canonical
//! (case-insensitive) pool-entry name, so differently spelled model names share one limit.

use zeph_orchestration::{
    AdmissionGate, AgentRouter, DagScheduler, SchedulerAction, TaskGraph, TaskNode,
};
use zeph_subagent::{ModelSpec, SubAgentDef};

struct ByTaskId;

impl AgentRouter for ByTaskId {
    fn route(&self, task: &TaskNode, available: &[SubAgentDef]) -> Option<String> {
        available.get(task.id.0 as usize).map(|d| d.name.clone())
    }
}

fn def(name: &str, provider: &str) -> SubAgentDef {
    let mut d = SubAgentDef::for_test(name);
    d.model = Some(ModelSpec::Named(provider.to_owned()));
    d
}

fn spawn_count(defs: Vec<SubAgentDef>) -> usize {
    let mut graph = TaskGraph::new("goal");
    graph.tasks = (0..2)
        .map(|i| TaskNode::new(i, format!("t{i}"), format!("task {i}")))
        .collect();
    let config = zeph_config::OrchestrationConfig {
        enabled: true,
        ..Default::default()
    };
    let gate = AdmissionGate::new(&[("quality".to_owned(), 1)]);
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(ByTaskId), defs, Some(gate)).unwrap();
    scheduler
        .tick()
        .iter()
        .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
        .count()
}

#[test]
fn test_admission_gate_shares_limit_across_provider_name_spellings() {
    assert_eq!(
        spawn_count(vec![def("lower", "quality"), def("upper", "Quality")]),
        1,
        "both definitions resolve to pool entry `quality` (limit 1)"
    );
}

#[test]
fn test_admission_gate_same_spelling_still_shares_limit() {
    assert_eq!(
        spawn_count(vec![def("a", "quality"), def("b", "quality")]),
        1
    );
}
