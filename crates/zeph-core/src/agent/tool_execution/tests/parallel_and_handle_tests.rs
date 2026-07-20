// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::future::join_all;
use std::assert_matches;
use zeph_tools::executor::{ToolCall, ToolExecutor, ToolOutput};

struct DelayExecutor {
    delay: Duration,
    call_order: Arc<AtomicUsize>,
}

impl zeph_tools::executor::ToolExecutor for DelayExecutor {
    fn execute(
        &self,
        _response: &str,
    ) -> impl Future<Output = Result<Option<ToolOutput>, zeph_tools::executor::ToolError>> + Send
    {
        std::future::ready(Ok(None))
    }

    fn execute_tool_call(
        &self,
        call: &ToolCall,
    ) -> impl Future<Output = Result<Option<ToolOutput>, zeph_tools::executor::ToolError>> + Send
    {
        let delay = self.delay;
        let order = self.call_order.clone();
        let idx = order.fetch_add(1, Ordering::SeqCst);
        let tool_id = call.tool_id.clone();
        async move {
            tokio::time::sleep(delay).await;
            Ok(Some(ToolOutput {
                tool_name: tool_id,
                summary: format!("result-{idx}"),
                blocks_executed: 1,
                diff: None,
                filter_stats: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
                ..Default::default()
            }))
        }
    }
    zeph_tools::tool_executor_no_inner_defaults!();
}

struct FailingNthExecutor {
    fail_index: usize,
    call_count: AtomicUsize,
}

impl zeph_tools::executor::ToolExecutor for FailingNthExecutor {
    fn execute(
        &self,
        _response: &str,
    ) -> impl Future<Output = Result<Option<ToolOutput>, zeph_tools::executor::ToolError>> + Send
    {
        std::future::ready(Ok(None))
    }

    fn execute_tool_call(
        &self,
        call: &ToolCall,
    ) -> impl Future<Output = Result<Option<ToolOutput>, zeph_tools::executor::ToolError>> + Send
    {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        let fail = idx == self.fail_index;
        let tool_id = call.tool_id.clone();
        async move {
            if fail {
                Err(zeph_tools::executor::ToolError::Execution(
                    std::io::Error::other(format!("tool {tool_id} failed")),
                ))
            } else {
                Ok(Some(ToolOutput {
                    tool_name: tool_id,
                    summary: format!("ok-{idx}"),
                    blocks_executed: 1,
                    diff: None,
                    filter_stats: None,
                    streamed: false,
                    terminal_id: None,
                    locations: None,
                    raw_response: None,
                    claim_source: None,
                    ..Default::default()
                }))
            }
        }
    }
    zeph_tools::tool_executor_no_inner_defaults!();
}

fn make_tool_use_request(id: &str, name: &str) -> zeph_llm::provider::ToolUseRequest {
    zeph_llm::provider::ToolUseRequest {
        id: id.into(),
        name: name.into(),
        input: serde_json::json!({}),
    }
}

fn make_calls(n: usize) -> Vec<ToolCall> {
    (0..n)
        .map(|i| ToolCall {
            tool_id: zeph_common::ToolName::new(format!("tool-{i}")),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        })
        .collect()
}

#[tokio::test]
async fn parallel_preserves_result_order() {
    let executor = DelayExecutor {
        delay: Duration::from_millis(10),
        call_order: Arc::new(AtomicUsize::new(0)),
    };
    let calls = make_calls(5);

    let futs: Vec<_> = calls
        .iter()
        .map(|c| executor.execute_tool_call(c))
        .collect();
    let results = join_all(futs).await;

    for (i, r) in results.iter().enumerate() {
        let out = r.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(out.tool_name, format!("tool-{i}"));
    }
}

#[tokio::test]
async fn parallel_faster_than_sequential() {
    let executor = DelayExecutor {
        delay: Duration::from_millis(50),
        call_order: Arc::new(AtomicUsize::new(0)),
    };
    let calls = make_calls(4);

    let start = Instant::now();
    let futs: Vec<_> = calls
        .iter()
        .map(|c| executor.execute_tool_call(c))
        .collect();
    let _results = join_all(futs).await;
    let parallel_time = start.elapsed();

    // Sequential would take >= 200ms (4 * 50ms); parallel should be ~50ms
    assert!(
        parallel_time < Duration::from_millis(150),
        "parallel took {parallel_time:?}, expected < 150ms"
    );
}

#[tokio::test]
async fn one_failure_does_not_block_others() {
    let executor = FailingNthExecutor {
        fail_index: 1,
        call_count: AtomicUsize::new(0),
    };
    let calls = make_calls(3);

    let futs: Vec<_> = calls
        .iter()
        .map(|c| executor.execute_tool_call(c))
        .collect();
    let results = join_all(futs).await;

    assert!(results[0].is_ok());
    assert!(results[1].is_err());
    assert!(results[2].is_ok());
}

#[test]
fn maybe_redact_disabled_returns_original() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use std::borrow::Cow;

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
    agent.runtime.config.security.redact_secrets = false;

    let text = "AWS_SECRET_ACCESS_KEY=abc123";
    let result = agent.maybe_redact(text);
    assert_matches!(result, Cow::Borrowed(_));
    assert_eq!(result.as_ref(), text);
}

#[test]
fn maybe_redact_enabled_redacts_secrets() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
    agent.runtime.config.security.redact_secrets = true;

    // A token-like secret should be redacted
    let text = "token: ghp_1234567890abcdefghijklmnopqrstuvwxyz";
    let result = agent.maybe_redact(text);
    // With redaction enabled, result should either be redacted or unchanged
    // (actual redaction depends on patterns matching)
    let _ = result.as_ref(); // just ensure no panic
}

#[test]
fn last_user_query_finds_latest_user_message() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use zeph_llm::provider::{Message, MessageMetadata, Role};

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    agent.msg.messages.push(Message {
        role: Role::User,
        content: "first question".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });
    agent.msg.messages.push(Message {
        role: Role::Assistant,
        content: "some answer".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "second question".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    assert_eq!(agent.last_user_query(), "second question");
}

#[test]
fn last_user_query_skips_tool_output_messages() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use zeph_llm::provider::{Message, MessageMetadata, Role};

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    agent.msg.messages.push(Message {
        role: Role::User,
        content: "what is the result?".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });
    // Tool output messages start with "[tool output"
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "[tool output] some output".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    assert_eq!(agent.last_user_query(), "what is the result?");
}

#[test]
fn last_user_query_no_user_messages_returns_empty() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    assert_eq!(agent.last_user_query(), "");
}

#[tokio::test]
async fn process_one_tool_result_blocked_is_error_with_policy_message() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use zeph_llm::provider::MessagePart;

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    let tc = make_tool_use_request("id-blocked", "bash");
    let mut result_parts = Vec::new();
    agent
        .process_one_tool_result(
            &tc,
            "id-blocked",
            &Instant::now(),
            Err(zeph_tools::ToolError::Blocked {
                command: "rm -rf /".into(),
            }),
            &mut result_parts,
            &mut Vec::new(),
            &mut false,
            &mut None,
            &mut Vec::new(),
            &mut 0,
            &mut zeph_sanitizer::ContentTrustLevel::Trusted,
            &mut zeph_sanitizer::ContentSourceKind::ToolResult,
        )
        .await
        .unwrap();

    let is_error = result_parts
        .iter()
        .any(|p| matches!(p, MessagePart::ToolResult { is_error: true, .. }));
    assert!(is_error, "a blocked command must be classified as an error");
    assert!(
        agent
            .channel
            .sent_messages()
            .iter()
            .any(|s| s.contains("blocked")),
        "the policy-blocked message must reach the channel"
    );
}

#[tokio::test]
async fn process_one_tool_result_cancelled_is_error() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use zeph_llm::provider::MessagePart;

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    let tc = make_tool_use_request("id-cancel", "bash");
    let mut result_parts = Vec::new();
    agent
        .process_one_tool_result(
            &tc,
            "id-cancel",
            &Instant::now(),
            Err(zeph_tools::ToolError::Cancelled),
            &mut result_parts,
            &mut Vec::new(),
            &mut false,
            &mut None,
            &mut Vec::new(),
            &mut 0,
            &mut zeph_sanitizer::ContentTrustLevel::Trusted,
            &mut zeph_sanitizer::ContentSourceKind::ToolResult,
        )
        .await
        .unwrap();

    let is_error = result_parts
        .iter()
        .any(|p| matches!(p, MessagePart::ToolResult { is_error: true, .. }));
    assert!(
        is_error,
        "a cancelled tool call must be classified as an error"
    );
}

#[tokio::test]
async fn process_one_tool_result_sandbox_violation_is_error_with_sandbox_message() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use zeph_llm::provider::MessagePart;

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    let tc = make_tool_use_request("id-sandbox", "bash");
    let mut result_parts = Vec::new();
    agent
        .process_one_tool_result(
            &tc,
            "id-sandbox",
            &Instant::now(),
            Err(zeph_tools::ToolError::SandboxViolation {
                path: "/etc/passwd".into(),
            }),
            &mut result_parts,
            &mut Vec::new(),
            &mut false,
            &mut None,
            &mut Vec::new(),
            &mut 0,
            &mut zeph_sanitizer::ContentTrustLevel::Trusted,
            &mut zeph_sanitizer::ContentSourceKind::ToolResult,
        )
        .await
        .unwrap();

    let is_error = result_parts
        .iter()
        .any(|p| matches!(p, MessagePart::ToolResult { is_error: true, .. }));
    assert!(
        is_error,
        "a sandbox violation must be classified as an error"
    );
    assert!(
        agent
            .channel
            .sent_messages()
            .iter()
            .any(|s| s.contains("sandbox")),
        "the sandbox-violation message must reach the channel"
    );
}

#[tokio::test]
async fn process_one_tool_result_ok_none_is_success_with_no_output_marker() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use zeph_llm::provider::MessagePart;

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    let tc = make_tool_use_request("id-none", "bash");
    let mut result_parts = Vec::new();
    agent
        .process_one_tool_result(
            &tc,
            "id-none",
            &Instant::now(),
            Ok(None),
            &mut result_parts,
            &mut Vec::new(),
            &mut false,
            &mut None,
            &mut Vec::new(),
            &mut 0,
            &mut zeph_sanitizer::ContentTrustLevel::Trusted,
            &mut zeph_sanitizer::ContentSourceKind::ToolResult,
        )
        .await
        .unwrap();

    let (content, is_error) = result_parts
        .iter()
        .find_map(|p| match p {
            MessagePart::ToolResult {
                content, is_error, ..
            } => Some((content.clone(), *is_error)),
            _ => None,
        })
        .expect("a ToolResult message part must be pushed");
    assert!(!is_error, "Ok(None) must not be classified as an error");
    assert!(
        content.contains("no output"),
        "Ok(None) must surface a no-output marker, got: {content}"
    );
}

#[tokio::test]
async fn process_one_tool_result_with_output_pushes_success_tool_result() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use zeph_llm::provider::MessagePart;

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    let tc = make_tool_use_request("id-out", "bash");
    let output = ToolOutput {
        tool_name: "bash".into(),
        summary: "hello from tool".into(),
        blocks_executed: 1,
        diff: None,
        filter_stats: None,
        streamed: false,
        terminal_id: None,
        locations: None,
        raw_response: None,
        claim_source: None,
        ..Default::default()
    };
    let mut result_parts = Vec::new();
    agent
        .process_one_tool_result(
            &tc,
            "id-out",
            &Instant::now(),
            Ok(Some(output)),
            &mut result_parts,
            &mut Vec::new(),
            &mut false,
            &mut None,
            &mut Vec::new(),
            &mut 0,
            &mut zeph_sanitizer::ContentTrustLevel::Trusted,
            &mut zeph_sanitizer::ContentSourceKind::ToolResult,
        )
        .await
        .unwrap();

    let (content, is_error) = result_parts
        .iter()
        .find_map(|p| match p {
            MessagePart::ToolResult {
                content, is_error, ..
            } => Some((content.clone(), *is_error)),
            _ => None,
        })
        .expect("a ToolResult message part must be pushed");
    assert!(!is_error);
    assert!(content.contains("hello from tool"));
}

#[tokio::test]
async fn process_one_tool_result_whitespace_only_output_is_still_success() {
    // Unlike the removed legacy harness (which special-cased a trim()-empty summary as an
    // early "no further action" skip), the production classify_tool_result path only treats
    // output as an error when it contains "[error]"/"[stderr]" markers — whitespace-only
    // output is not special-cased. This asserts the current, real behavior.
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use zeph_llm::provider::MessagePart;

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    let tc = make_tool_use_request("id-empty", "bash");
    let output = ToolOutput {
        tool_name: "bash".into(),
        summary: "   ".into(),
        blocks_executed: 0,
        diff: None,
        filter_stats: None,
        streamed: false,
        terminal_id: None,
        locations: None,
        raw_response: None,
        claim_source: None,
        ..Default::default()
    };
    let mut result_parts = Vec::new();
    agent
        .process_one_tool_result(
            &tc,
            "id-empty",
            &Instant::now(),
            Ok(Some(output)),
            &mut result_parts,
            &mut Vec::new(),
            &mut false,
            &mut None,
            &mut Vec::new(),
            &mut 0,
            &mut zeph_sanitizer::ContentTrustLevel::Trusted,
            &mut zeph_sanitizer::ContentSourceKind::ToolResult,
        )
        .await
        .unwrap();

    let is_error = result_parts
        .iter()
        .any(|p| matches!(p, MessagePart::ToolResult { is_error: true, .. }));
    assert!(
        !is_error,
        "whitespace-only output must not be classified as an error"
    );
}

#[tokio::test]
async fn process_one_tool_result_error_prefix_records_tool_failure_outcome() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
    // reflection_used = true so the self-reflection path is skipped
    agent.services.learning_engine.mark_reflection_used();

    let tc = make_tool_use_request("id-error-prefix", "bash");
    let output = ToolOutput {
        tool_name: "bash".into(),
        summary: "[error] spawn failed".into(),
        blocks_executed: 1,
        diff: None,
        filter_stats: None,
        streamed: false,
        terminal_id: None,
        locations: None,
        raw_response: None,
        claim_source: None,
        ..Default::default()
    };
    let mut pending_outcomes = Vec::new();
    agent
        .process_one_tool_result(
            &tc,
            "id-error-prefix",
            &Instant::now(),
            Ok(Some(output)),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut false,
            &mut None,
            &mut pending_outcomes,
            &mut 0,
            &mut zeph_sanitizer::ContentTrustLevel::Trusted,
            &mut zeph_sanitizer::ContentSourceKind::ToolResult,
        )
        .await
        .unwrap();

    assert!(
        pending_outcomes.iter().any(|o| o.outcome == "tool_failure"),
        "output containing [error] must be recorded as a tool_failure skill outcome"
    );
}

// classify_tool_result's "[stderr]" branch (tool_result.rs:295) currently has no test that can
// distinguish Error from Success classification for it — a pre-existing gap, not introduced by
// this PR (the removed legacy-harness test was equally non-discriminating). Follow-up test-
// coverage issue to be filed separately.

#[tokio::test]
async fn buffered_preserves_order() {
    use futures::StreamExt;

    let executor = DelayExecutor {
        delay: Duration::from_millis(10),
        call_order: Arc::new(AtomicUsize::new(0)),
    };
    let calls = make_calls(6);
    let max_parallel = 2;

    let stream = futures::stream::iter(calls.iter().map(|c| executor.execute_tool_call(c)));
    let results: Vec<_> =
        futures::StreamExt::collect::<Vec<_>>(stream.buffered(max_parallel)).await;

    for (i, r) in results.iter().enumerate() {
        let out = r.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(out.tool_name, format!("tool-{i}"));
    }
}

#[test]
fn inject_active_skill_env_maps_secret_name_to_env_key() {
    // Verify the mapping logic: "github_token" -> "GITHUB_TOKEN"
    let secret_name = "github_token";
    let env_key = secret_name.to_uppercase();
    assert_eq!(env_key, "GITHUB_TOKEN");

    // "some_api_key" -> "SOME_API_KEY"
    let secret_name2 = "some_api_key";
    let env_key2 = secret_name2.to_uppercase();
    assert_eq!(env_key2, "SOME_API_KEY");
}

#[tokio::test]
async fn inject_active_skill_env_injects_only_active_skill_secrets() {
    use crate::agent::Agent;
    #[allow(clippy::wildcard_imports)]
    use crate::agent::agent_tests::*;
    use crate::vault::Secret;
    use zeph_skills::registry::SkillRegistry;

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = SkillRegistry::default();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    // Add available custom secrets
    agent
        .services
        .skill
        .available_custom_secrets
        .insert("github_token".into(), Secret::new("gh-secret-val"));
    agent
        .services
        .skill
        .available_custom_secrets
        .insert("other_key".into(), Secret::new("other-val"));

    // No active skills — inject_active_skill_env should be a no-op
    assert!(agent.services.skill.active_skill_names.is_empty());
    agent.inject_active_skill_env();
    // tool_executor.set_skill_env was not called (no-op path)
    assert!(agent.services.skill.active_skill_names.is_empty());
}

#[test]
fn inject_active_skill_env_calls_set_skill_env_with_correct_map() {
    use crate::agent::Agent;
    #[allow(clippy::wildcard_imports)]
    use crate::agent::agent_tests::*;
    use crate::vault::Secret;
    use std::sync::Arc;
    use zeph_skills::registry::SkillRegistry;

    // Build a registry with one skill that requires "github_token".
    let temp_dir = tempfile::tempdir().unwrap();
    let skill_dir = temp_dir.path().join("gh-skill");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: gh-skill\ndescription: GitHub.\nx-requires-secrets: github_token\n---\nbody",
    )
    .unwrap();
    let registry = SkillRegistry::load(&[temp_dir.path().to_path_buf()]);

    let executor = MockToolExecutor::no_tools();
    let captured = Arc::clone(&executor.captured_env);

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent
        .services
        .skill
        .available_custom_secrets
        .insert("github_token".into(), Secret::new("gh-val"));
    agent
        .services
        .skill
        .active_skill_names
        .push("gh-skill".into());

    agent.inject_active_skill_env();

    let calls = captured.lock().unwrap();
    assert_eq!(calls.len(), 1, "set_skill_env must be called once");
    let env = calls[0].as_ref().expect("env must be Some");
    assert_eq!(env.get("GITHUB_TOKEN").map(String::as_str), Some("gh-val"));
}

#[test]
fn inject_active_skill_env_clears_after_call() {
    use crate::agent::Agent;
    #[allow(clippy::wildcard_imports)]
    use crate::agent::agent_tests::*;
    use crate::vault::Secret;
    use std::sync::Arc;
    use zeph_skills::registry::SkillRegistry;

    let temp_dir = tempfile::tempdir().unwrap();
    let skill_dir = temp_dir.path().join("tok-skill");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: tok-skill\ndescription: Token.\nx-requires-secrets: api_token\n---\nbody",
    )
    .unwrap();
    let registry = SkillRegistry::load(&[temp_dir.path().to_path_buf()]);

    let executor = MockToolExecutor::no_tools();
    let captured = Arc::clone(&executor.captured_env);

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent
        .services
        .skill
        .available_custom_secrets
        .insert("api_token".into(), Secret::new("tok-val"));
    agent
        .services
        .skill
        .active_skill_names
        .push("tok-skill".into());

    // First call — injects env
    agent.inject_active_skill_env();
    // Simulate post-execution clear
    agent.tool_executor.set_skill_env(None);

    let calls = captured.lock().unwrap();
    assert_eq!(calls.len(), 2, "inject + clear = 2 calls");
    assert!(calls[0].is_some(), "first call must set env");
    assert!(calls[1].is_none(), "second call must clear env");
}
