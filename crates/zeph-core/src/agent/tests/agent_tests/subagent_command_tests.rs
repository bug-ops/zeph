// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[allow(unused_imports)]
use super::*;

// ── handle_agent_command tests ────────────────────────────────────────────

use zeph_subagent::AgentCommand;

fn make_agent_with_manager() -> Agent<MockChannel> {
    use zeph_subagent::def::{SkillFilter, SubAgentPermissions, ToolPolicy};
    use zeph_subagent::hooks::SubagentHooks;
    use zeph_subagent::{SubAgentDef, SubAgentManager};

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let mut mgr = SubAgentManager::new(4);
    mgr.definitions_mut().push(SubAgentDef {
        name: "helper".into(),
        description: "A helper bot".into(),
        model: None,
        tools: ToolPolicy::InheritAll,
        disallowed_tools: vec![],
        permissions: SubAgentPermissions::default(),
        skills: SkillFilter::default(),
        system_prompt: "You are helpful.".into(),
        hooks: SubagentHooks::default(),
        memory: None,
        source: None,
        file_path: None,
    });
    agent.orchestration.subagent_manager = Some(mgr);
    agent
}

#[tokio::test]
async fn agent_command_no_manager_returns_none() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    // no subagent_manager set — List needs manager to return Some
    assert!(
        agent
            .handle_agent_command(AgentCommand::List)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn agent_command_list_returns_definitions() {
    let mut agent = make_agent_with_manager();
    let resp = agent
        .handle_agent_command(AgentCommand::List)
        .await
        .unwrap();
    assert!(resp.contains("helper"));
    assert!(resp.contains("A helper bot"));
}

#[tokio::test]
async fn agent_command_spawn_unknown_name_returns_error() {
    let mut agent = make_agent_with_manager();
    let resp = agent
        .handle_agent_command(AgentCommand::Background {
            name: "unknown-bot".into(),
            prompt: "do something".into(),
        })
        .await
        .unwrap();
    assert!(resp.contains("Failed to spawn"));
}

#[tokio::test]
async fn agent_command_spawn_known_name_returns_started() {
    let mut agent = make_agent_with_manager();
    let resp = agent
        .handle_agent_command(AgentCommand::Background {
            name: "helper".into(),
            prompt: "do some work".into(),
        })
        .await
        .unwrap();
    assert!(resp.contains("helper"));
    assert!(resp.contains("started"));
}

#[tokio::test]
async fn agent_command_status_no_agents_returns_empty_message() {
    let mut agent = make_agent_with_manager();
    let resp = agent
        .handle_agent_command(AgentCommand::Status)
        .await
        .unwrap();
    assert!(resp.contains("No active sub-agents"));
}

#[tokio::test]
async fn agent_command_cancel_unknown_id_returns_not_found() {
    let mut agent = make_agent_with_manager();
    let resp = agent
        .handle_agent_command(AgentCommand::Cancel {
            id: "deadbeef".into(),
        })
        .await
        .unwrap();
    assert!(resp.contains("No sub-agent"));
}

#[tokio::test]
async fn agent_command_cancel_valid_id_succeeds() {
    let mut agent = make_agent_with_manager();
    // spawn first so we have a task to cancel
    let spawn_resp = agent
        .handle_agent_command(AgentCommand::Background {
            name: "helper".into(),
            prompt: "cancel this".into(),
        })
        .await
        .unwrap();
    // extract short id from "started in background (id: XXXXXXXX)"
    let short_id = spawn_resp
        .split("id: ")
        .nth(1)
        .unwrap()
        .trim_end_matches(')')
        .trim()
        .to_string();
    let resp = agent
        .handle_agent_command(AgentCommand::Cancel { id: short_id })
        .await
        .unwrap();
    assert!(resp.contains("Cancelled"));
}

#[tokio::test]
async fn agent_command_approve_no_pending_request() {
    let mut agent = make_agent_with_manager();
    // Spawn an agent first so there's an active agent to reference
    let spawn_resp = agent
        .handle_agent_command(AgentCommand::Background {
            name: "helper".into(),
            prompt: "do work".into(),
        })
        .await
        .unwrap();
    let short_id = spawn_resp
        .split("id: ")
        .nth(1)
        .unwrap()
        .trim_end_matches(')')
        .trim()
        .to_string();
    let resp = agent
        .handle_agent_command(AgentCommand::Approve { id: short_id })
        .await
        .unwrap();
    assert!(resp.contains("No pending secret request"));
}
