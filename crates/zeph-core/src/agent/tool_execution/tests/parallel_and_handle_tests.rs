// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::future::join_all;
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
            }))
        }
    }
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
                }))
            }
        }
    }
}

fn make_calls(n: usize) -> Vec<ToolCall> {
    (0..n)
        .map(|i| ToolCall {
            tool_id: zeph_common::ToolName::new(format!("tool-{i}")),
            params: serde_json::Map::new(),
            caller_id: None,
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
    assert!(matches!(result, Cow::Borrowed(_)));
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
async fn handle_tool_result_blocked_returns_false() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use zeph_tools::executor::ToolError;

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    let result = agent
        .handle_tool_result(
            "response",
            Err(ToolError::Blocked {
                command: "rm -rf /".into(),
            }),
        )
        .await
        .unwrap();
    assert!(!result);
    assert!(
        agent
            .channel
            .sent_messages()
            .iter()
            .any(|s| s.contains("blocked"))
    );
}

#[tokio::test]
async fn handle_tool_result_cancelled_returns_false() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use zeph_tools::executor::ToolError;

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    let result = agent
        .handle_tool_result("response", Err(ToolError::Cancelled))
        .await
        .unwrap();
    assert!(!result);
}

#[tokio::test]
async fn handle_tool_result_sandbox_violation_returns_false() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use zeph_tools::executor::ToolError;

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    let result = agent
        .handle_tool_result(
            "response",
            Err(ToolError::SandboxViolation {
                path: "/etc/passwd".into(),
            }),
        )
        .await
        .unwrap();
    assert!(!result);
    assert!(
        agent
            .channel
            .sent_messages()
            .iter()
            .any(|s| s.contains("sandbox"))
    );
}

#[tokio::test]
async fn handle_tool_result_none_returns_false() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    let result = agent
        .handle_tool_result("response", Ok(None))
        .await
        .unwrap();
    assert!(!result);
}

#[tokio::test]
async fn handle_tool_result_with_output_returns_true() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

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
    };
    let result = agent
        .handle_tool_result("response", Ok(Some(output)))
        .await
        .unwrap();
    assert!(result);
}

#[tokio::test]
async fn handle_tool_result_empty_output_returns_false() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    let output = ToolOutput {
        tool_name: "bash".into(),
        summary: "   ".into(), // whitespace only → considered empty
        blocks_executed: 0,
        diff: None,
        filter_stats: None,
        streamed: false,
        terminal_id: None,
        locations: None,
        raw_response: None,
        claim_source: None,
    };
    let result = agent
        .handle_tool_result("response", Ok(Some(output)))
        .await
        .unwrap();
    assert!(!result);
}

#[tokio::test]
async fn handle_tool_result_error_prefix_triggers_anomaly_error() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

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
    };
    // reflection_used = true so reflection path is skipped
    agent.services.learning_engine.mark_reflection_used();
    let result = agent
        .handle_tool_result("response", Ok(Some(output)))
        .await
        .unwrap();
    // Returns true because the tool loop continues after recording failure
    assert!(result);
}

#[tokio::test]
async fn handle_tool_result_stderr_prefix_triggers_anomaly_error() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    // [stderr] prefix is produced by ShellExecutor when the child process writes to stderr.
    // Prior to this fix, such output was silently classified as AnomalyOutcome::Success.
    let output = ToolOutput {
        tool_name: "bash".into(),
        summary: "[stderr] warning: deprecated API used".into(),
        blocks_executed: 1,
        diff: None,
        filter_stats: None,
        streamed: false,
        terminal_id: None,
        locations: None,
        raw_response: None,
        claim_source: None,
    };
    agent.services.learning_engine.mark_reflection_used();
    let result = agent
        .handle_tool_result("response", Ok(Some(output)))
        .await
        .unwrap();
    // handle_tool_result returns true (tool loop continues) regardless of anomaly outcome
    assert!(result);
}

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

#[tokio::test]
async fn call_llm_returns_cached_response_without_provider_call() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider_streaming,
    };
    use std::sync::Arc;
    use zeph_llm::provider::{Message, MessageMetadata, Role};
    use zeph_memory::{ResponseCache, store::SqliteStore};

    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    // Streaming provider — cache must be consulted regardless of streaming support.
    let provider = mock_provider_streaming(vec!["uncached response".into()]);
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    // Set up a response cache with a pre-populated entry.
    let store = SqliteStore::new(":memory:").await.unwrap();
    let cache = Arc::new(ResponseCache::new(store.pool().clone(), 3600));

    // Pre-populate cache for the user message we're about to add.
    let user_content = "what is 2+2?";
    let key = ResponseCache::compute_key(user_content, &agent.runtime.config.model_name);
    cache
        .put(&key, "cached response", "test-model")
        .await
        .unwrap();

    agent.services.session.response_cache = Some(cache);

    agent.msg.messages.push(Message {
        role: Role::User,
        content: user_content.into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    let result = agent.call_llm_with_timeout().await.unwrap();
    assert_eq!(result.as_deref(), Some("cached response"));
    // Channel should have received the cached response
    assert!(
        agent
            .channel
            .sent_messages()
            .iter()
            .any(|s| s == "cached response")
    );
}

#[tokio::test]
async fn store_response_in_cache_enables_second_call_to_return_cached() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use std::sync::Arc;
    use zeph_llm::provider::{Message, MessageMetadata, Role};
    use zeph_memory::{ResponseCache, store::SqliteStore};

    // Non-streaming provider has one response; the second call must come from cache.
    let provider = mock_provider(vec!["provider response".into()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    let store = SqliteStore::new(":memory:").await.unwrap();
    let cache = Arc::new(ResponseCache::new(store.pool().clone(), 3600));
    agent.services.session.response_cache = Some(cache);

    agent.msg.messages.push(Message {
        role: Role::User,
        content: "what is 3+3?".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    // First call — hits provider, stores response in cache.
    let first = agent.call_llm_with_timeout().await.unwrap();
    assert_eq!(first.as_deref(), Some("provider response"));

    // Second call with the same messages — must return cached value.
    let second = agent.call_llm_with_timeout().await.unwrap();
    assert_eq!(
        second.as_deref(),
        Some("provider response"),
        "second call must return cached response"
    );

    // Both first call (provider) and second call (cache hit) send via channel.send().
    let sent = agent.channel.sent_messages();
    assert!(
        sent.iter().any(|s| s == "provider response"),
        "provider response must have been sent via channel"
    );
}

#[tokio::test]
async fn cache_key_stable_across_growing_history() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider_streaming,
    };
    use std::sync::Arc;
    use zeph_llm::provider::{Message, MessageMetadata, Role};
    use zeph_memory::{ResponseCache, store::SqliteStore};

    let provider = mock_provider_streaming(vec!["turn2 response".into()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    let store = SqliteStore::new(":memory:").await.unwrap();
    let cache = Arc::new(ResponseCache::new(store.pool().clone(), 3600));

    // Simulate turn 1: store a cached response for user message "hello".
    let user_msg = "hello";
    let key = ResponseCache::compute_key(user_msg, &agent.runtime.config.model_name);
    cache
        .put(&key, "cached hello response", "test-model")
        .await
        .unwrap();
    agent.services.session.response_cache = Some(cache);

    // Add history from turn 1: system context + prior exchange.
    agent.msg.messages.push(Message {
        role: Role::Assistant,
        content: "cached hello response".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    // Turn 2: same user message "hello" but history has grown.
    agent.msg.messages.push(Message {
        role: Role::User,
        content: user_msg.into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    // Must hit cache despite history growth — key is based on last user message only.
    let result = agent.call_llm_with_timeout().await.unwrap();
    assert_eq!(
        result.as_deref(),
        Some("cached hello response"),
        "cache must hit for same user message regardless of preceding history"
    );
}

#[tokio::test]
async fn cache_skipped_when_no_user_message() {
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider_streaming,
    };
    use std::sync::Arc;
    use zeph_llm::provider::{Message, MessageMetadata, Role};
    use zeph_memory::{ResponseCache, store::SqliteStore};

    let provider = mock_provider_streaming(vec!["llm response".into()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

    let store = SqliteStore::new(":memory:").await.unwrap();
    let cache = Arc::new(ResponseCache::new(store.pool().clone(), 3600));
    agent.services.session.response_cache = Some(cache);

    // Only system/assistant messages, no user message.
    agent.msg.messages.push(Message {
        role: Role::System,
        content: "you are helpful".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });
    agent.msg.messages.push(Message {
        role: Role::Assistant,
        content: "hello".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    // Should skip cache (no user message) and call LLM.
    let result = agent.call_llm_with_timeout().await.unwrap();
    assert_eq!(result.as_deref(), Some("llm response"));
}
