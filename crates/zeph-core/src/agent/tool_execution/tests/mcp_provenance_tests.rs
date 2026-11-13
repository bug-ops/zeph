// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tool results are classified by executor provenance (the MCP registry and the executor's own
//! claim), never by the shape of the tool name.

use zeph_llm::provider::{MessagePart, ToolUseRequest};
use zeph_sanitizer::{ContentIsolationConfig, ContentSanitizer, ContentTrustLevel};
use zeph_tools::ClaimSource;
use zeph_tools::executor::{ToolError, ToolOutput};

use crate::agent::Agent;
use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, github_mcp_tool, github_mcp_tool_ids,
    mcp_tool_def, mock_provider,
};

const EXTERNAL: u8 = ContentTrustLevel::ExternalUntrusted as u8;

fn output(claim_source: Option<ClaimSource>) -> ToolOutput {
    ToolOutput {
        tool_name: "github:create_issue".into(),
        summary: "issue created".into(),
        blocks_executed: 1,
        claim_source,
        ..Default::default()
    }
}

fn failure() -> Result<Option<ToolOutput>, ToolError> {
    Err(ToolError::Execution(std::io::Error::other(
        "upstream failure",
    )))
}

fn agent_with(executor: MockToolExecutor) -> Agent<MockChannel> {
    let mut agent = Agent::new(
        mock_provider(vec![]),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        executor,
    );
    agent.services.security.sanitizer = ContentSanitizer::new(&ContentIsolationConfig {
        enabled: true,
        spotlight_untrusted: true,
        flag_injection_patterns: false,
        ..Default::default()
    });
    agent
}

/// Attach a single-writer tool list and return the registry handle consumers read.
fn attach_mcp_tool_list(agent: &mut Agent<MockChannel>) -> zeph_tools::McpToolIdsHandle {
    let (writer, _reader) = zeph_mcp::McpToolList::new(Vec::new());
    let handle = writer.ids_handle();
    agent.services.mcp.tool_list = Some(writer);
    handle
}

fn call(name: &str) -> Vec<ToolUseRequest> {
    vec![ToolUseRequest {
        id: "call-1".into(),
        name: name.into(),
        input: serde_json::json!({}),
    }]
}

/// Content and batch trust tag of the tool-result message the turn produced.
fn tool_result_message(agent: &Agent<MockChannel>) -> (String, Option<u8>) {
    agent
        .msg
        .messages
        .iter()
        .rev()
        .find_map(|m| {
            m.parts.iter().find_map(|p| match p {
                MessagePart::ToolResult { content, .. } => {
                    Some((content.clone(), m.metadata.trust_level))
                }
                _ => None,
            })
        })
        .expect("a tool result message")
}

async fn run_turn(agent: &mut Agent<MockChannel>, tool: &str) -> (String, Option<u8>) {
    agent
        .handle_native_tool_calls(None, &call(tool))
        .await
        .unwrap();
    tool_result_message(agent)
}

fn assert_external(agent: &Agent<MockChannel>, result: &(String, Option<u8>)) {
    assert!(result.0.contains("<external-data"), "{}", result.0);
    assert_eq!(result.1, Some(EXTERNAL));
    assert_eq!(
        *agent.services.security.memory_consent_trust.read(),
        EXTERNAL
    );
}

#[tokio::test]
async fn stored_registry_entry_result_and_error_text_are_external() {
    let (id, _) = github_mcp_tool_ids();
    for outcome in [Ok(Some(output(None))), failure()] {
        let mut agent = agent_with(MockToolExecutor::new(vec![outcome]));
        attach_mcp_tool_list(&mut agent);
        agent.services.mcp.set_tools(vec![github_mcp_tool()]);
        let result = run_turn(&mut agent, &id).await;
        assert_external(&agent, &result);
    }
}

#[tokio::test]
async fn live_executor_definition_alone_classifies_error_text_as_external() {
    let (id, _) = github_mcp_tool_ids();
    let executor =
        MockToolExecutor::new(vec![failure()]).with_definitions(vec![mcp_tool_def(&id, "github")]);
    let mut agent = agent_with(executor);
    assert!(agent.services.mcp.tools().is_empty());
    let result = run_turn(&mut agent, &id).await;
    assert_external(&agent, &result);
}

#[tokio::test]
async fn mcp_claim_without_any_registry_entry_is_external() {
    let (id, _) = github_mcp_tool_ids();
    let mut agent = agent_with(MockToolExecutor::new(vec![Ok(Some(output(Some(
        ClaimSource::Mcp,
    ))))]));
    let result = run_turn(&mut agent, &id).await;
    assert_external(&agent, &result);
}

#[tokio::test]
async fn unregistered_native_tool_error_is_not_external() {
    let mut agent = agent_with(MockToolExecutor::new(vec![failure()]));
    let result = run_turn(&mut agent, "bash").await;
    assert!(!result.0.contains("<external-data"), "{}", result.0);
    assert_ne!(result.1, Some(EXTERNAL));
}

#[tokio::test]
async fn mcp_tool_sharing_an_internal_name_is_external() {
    let executor = MockToolExecutor::new(vec![Ok(Some(output(None)))])
        .with_definitions(vec![mcp_tool_def("memory_search", "notes")]);
    let mut agent = agent_with(executor);
    let result = run_turn(&mut agent, "memory_search").await;
    assert_external(&agent, &result);
}

#[test]
fn mcp_state_mutators_keep_the_id_registry_in_sync() {
    let (id, _) = github_mcp_tool_ids();
    let mut agent = agent_with(MockToolExecutor::no_tools());
    let handle = attach_mcp_tool_list(&mut agent);
    let state = &mut agent.services.mcp;

    state.set_tools(vec![github_mcp_tool()]);
    assert!(handle.contains(&id));
    assert_eq!(
        handle
            .snapshot()
            .server_id(&id)
            .map(zeph_common::types::McpServerId::as_str),
        Some("github")
    );

    state.retain_tools(|t| t.server_id != "github");
    assert!(!handle.contains(&id));

    state.extend_tools(vec![github_mcp_tool()]);
    assert!(handle.contains(&id));

    state.set_tools(Vec::new());
    assert!(!handle.contains(&id));
}

#[test]
fn long_ids_are_registered_in_their_truncated_dispatch_form() {
    let tool = zeph_mcp::McpTool {
        name: "n".repeat(200),
        ..github_mcp_tool()
    };
    let id = tool.sanitized_id();
    assert!(id.len() <= 128, "dispatch ids are capped at 128 chars");
    let mut agent = agent_with(MockToolExecutor::no_tools());
    let handle = attach_mcp_tool_list(&mut agent);
    agent.services.mcp.set_tools(vec![tool]);
    assert!(handle.contains(&id));
    assert_eq!(
        handle
            .snapshot()
            .server_id(&id)
            .map(zeph_common::types::McpServerId::as_str),
        Some("github")
    );
}

#[tokio::test]
async fn pruned_mcp_tool_is_still_classified_as_external() {
    let (id, _) = github_mcp_tool_ids();
    let mut agent = agent_with(MockToolExecutor::new(vec![Ok(Some(output(None)))]));
    attach_mcp_tool_list(&mut agent);
    agent.services.mcp.set_tools(vec![github_mcp_tool()]);
    agent.services.mcp.apply_pruned_tools(Vec::new());
    let result = run_turn(&mut agent, &id).await;
    assert_external(&agent, &result);
}

#[tokio::test]
async fn without_a_tool_list_the_live_definitions_are_the_fallback() {
    let (id, _) = github_mcp_tool_ids();
    let executor = MockToolExecutor::new(vec![Ok(Some(output(None)))])
        .with_definitions(vec![mcp_tool_def(&id, "github")]);
    let mut agent = agent_with(executor);
    assert!(agent.services.mcp.tool_list.is_none());
    let result = run_turn(&mut agent, &id).await;
    assert_external(&agent, &result);
}

#[tokio::test]
async fn long_id_from_a_live_executor_definition_classifies_as_external() {
    let tool = zeph_mcp::McpTool {
        name: "n".repeat(200),
        ..github_mcp_tool()
    };
    let id = tool.sanitized_id();
    let executor =
        MockToolExecutor::new(vec![failure()]).with_definitions(vec![mcp_tool_def(&id, "github")]);
    let mut agent = agent_with(executor);
    let result = run_turn(&mut agent, &id).await;
    assert_external(&agent, &result);
}

/// Records the memory-consent slot at the moment the tool executes.
struct SlotProbeExecutor {
    slot: crate::memory_tools::MemoryConsentTrustSlot,
    seen: std::sync::Arc<std::sync::Mutex<Option<u8>>>,
}

impl zeph_tools::ToolExecutor for SlotProbeExecutor {
    fn execute(
        &self,
        _response: &str,
    ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        std::future::ready(Ok(None))
    }

    fn execute_tool_call(
        &self,
        _call: &zeph_tools::ToolCall,
    ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        *self.seen.lock().unwrap() = Some(*self.slot.read());
        std::future::ready(Ok(Some(output(None))))
    }

    zeph_tools::tool_executor_no_inner_defaults!();
}

#[tokio::test]
async fn consent_slot_is_external_before_a_registered_mcp_call_executes() {
    let (id, _) = github_mcp_tool_ids();
    let slot = std::sync::Arc::new(parking_lot::RwLock::new(0u8));
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let executor = SlotProbeExecutor {
        slot: std::sync::Arc::clone(&slot),
        seen: std::sync::Arc::clone(&seen),
    };
    let mut agent = Agent::new(
        mock_provider(vec![]),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        executor,
    )
    .with_memory_consent_trust_slot(slot);
    attach_mcp_tool_list(&mut agent);
    agent.services.mcp.set_tools(vec![github_mcp_tool()]);
    agent
        .handle_native_tool_calls(None, &call(&id))
        .await
        .unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        Some(EXTERNAL),
        "the slot must already be ExternalUntrusted while the MCP call runs"
    );
}

#[tokio::test]
async fn reflection_text_of_a_failing_mcp_call_is_sanitized_as_mcp() {
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;

    let (id, _) = github_mcp_tool_ids();
    let (mock, recorded) =
        MockProvider::with_responses(vec!["reflection response".into()]).with_recording();
    let temp_dir = tempfile::tempdir().unwrap();
    let skill_dir = temp_dir.path().join("test-skill");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: test-skill\ndescription: A test skill\n---\nTest skill body",
    )
    .unwrap();
    let registry = zeph_skills::registry::SkillRegistry::load(&[temp_dir.path().to_path_buf()]);
    let mut agent = Agent::new(
        AnyProvider::Mock(mock),
        MockChannel::new(vec![]),
        registry,
        None,
        5,
        MockToolExecutor::new(vec![Err(ToolError::InvalidParams {
            message: "upstream failure".into(),
        })]),
    )
    .with_learning(crate::config::LearningConfig {
        enabled: true,
        ..crate::config::LearningConfig::default()
    });
    agent
        .services
        .skill
        .active_skill_names
        .push("test-skill".into());
    attach_mcp_tool_list(&mut agent);
    agent.services.mcp.set_tools(vec![github_mcp_tool()]);
    agent
        .handle_native_tool_calls(None, &call(&id))
        .await
        .unwrap();

    let sent = recorded.lock().unwrap();
    let reflection_prompt = sent
        .iter()
        .flatten()
        .map(|m| m.content.as_str())
        .find(|c| c.contains("upstream failure"))
        .expect("the reflection prompt carries the error text");
    assert!(
        reflection_prompt.contains("<external-data"),
        "{reflection_prompt}"
    );
}
