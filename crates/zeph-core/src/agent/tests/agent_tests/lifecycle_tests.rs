// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[allow(unused_imports)]
use super::*;

#[tokio::test]
async fn agent_new_initializes_with_system_prompt() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let agent = Agent::new(provider, channel, registry, None, 5, executor);

    assert_eq!(agent.msg.messages.len(), 1);
    assert_eq!(agent.msg.messages[0].role, Role::System);
    assert!(!agent.msg.messages[0].content.is_empty());
}

#[tokio::test]
async fn agent_with_working_dir_updates_environment_context() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let tmp = tempfile::tempdir().unwrap();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.runtime.model_name = "test-model".to_string();
    let agent = agent.with_working_dir(tmp.path().to_path_buf());

    assert_eq!(
        agent.session.env_context.working_dir,
        tmp.path().display().to_string()
    );
    assert_eq!(agent.session.env_context.model_name, "test-model");
}

#[tokio::test]
async fn agent_with_embedding_model_sets_model() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.skill_state.embedding_model = "test-embed-model".to_string();

    assert_eq!(agent.skill_state.embedding_model, "test-embed-model");
}

#[tokio::test]
async fn agent_with_shutdown_sets_receiver() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (_tx, rx) = watch::channel(false);

    let _agent = Agent::new(provider, channel, registry, None, 5, executor).with_shutdown(rx);
}

#[tokio::test]
async fn agent_with_security_sets_config() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let security = SecurityConfig {
        redact_secrets: true,
        ..Default::default()
    };
    let timeouts = TimeoutConfig {
        llm_seconds: 60,
        ..Default::default()
    };

    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_security(security, timeouts);

    assert!(agent.runtime.security.redact_secrets);
    assert_eq!(agent.runtime.timeouts.llm_seconds, 60);
}

#[tokio::test]
async fn agent_run_handles_empty_channel() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let result = agent.run().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn agent_run_processes_user_message() {
    let provider = mock_provider(vec!["test response".to_string()]);
    let channel = MockChannel::new(vec!["hello".to_string()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let result = agent.run().await;
    assert!(result.is_ok());
    assert_eq!(agent.msg.messages.len(), 3);
    assert_eq!(agent.msg.messages[1].role, Role::User);
    assert_eq!(agent.msg.messages[1].content, "hello");
    assert_eq!(agent.msg.messages[2].role, Role::Assistant);
}

#[tokio::test]
async fn agent_run_handles_shutdown_signal() {
    let provider = mock_provider(vec![]);
    let (tx, rx) = watch::channel(false);
    let channel = MockChannel::new(vec!["should not process".to_string()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_shutdown(rx);

    tx.send(true).unwrap();

    let result = agent.run().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn agent_handles_skills_command() {
    let provider = mock_provider(vec![]);
    let _channel = MockChannel::new(vec!["/skills".to_string()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let agent_channel = MockChannel::new(vec!["/skills".to_string()]);
    let sent = agent_channel.sent.clone();

    let mut agent = Agent::new(provider, agent_channel, registry, None, 5, executor);

    let result = agent.run().await;
    assert!(result.is_ok());

    let sent_msgs = sent.lock().unwrap();
    assert!(!sent_msgs.is_empty());
    assert!(sent_msgs[0].contains("Available skills"));
}

#[tokio::test]
async fn agent_process_response_handles_empty_response() {
    // In the native path, an empty LLM response is treated as a completed turn (no text
    // to display). The agent should complete without error and without sending a message.
    let provider = mock_provider(vec![String::new()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let agent_channel = MockChannel::new(vec!["test".to_string()]);

    let mut agent = Agent::new(provider, agent_channel, registry, None, 5, executor);

    let result = agent.run().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn agent_handles_tool_execution_success() {
    use zeph_llm::provider::{ChatResponse, ToolUseRequest};
    let tool_call = ToolUseRequest {
        id: "call1".into(),
        name: "bash".into(),
        input: serde_json::json!({}),
    };
    let (mock, _) = MockProvider::default().with_tool_use(vec![
        ChatResponse::ToolUse {
            text: None,
            tool_calls: vec![tool_call],
            thinking_blocks: vec![],
        },
        ChatResponse::Text("done".into()),
    ]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::new(vec![Ok(Some(ToolOutput {
        tool_name: "bash".into(),
        summary: "tool executed successfully".to_string(),
        blocks_executed: 1,
        filter_stats: None,
        diff: None,
        streamed: false,
        terminal_id: None,
        locations: None,
        raw_response: None,
        claim_source: None,
    }))]);

    let agent_channel = MockChannel::new(vec!["execute tool".to_string()]);
    let sent = agent_channel.sent.clone();

    let mut agent = Agent::new(
        AnyProvider::Mock(mock),
        agent_channel,
        registry,
        None,
        5,
        executor,
    );

    let result = agent.run().await;
    assert!(result.is_ok());

    let sent_msgs = sent.lock().unwrap();
    assert!(
        sent_msgs
            .iter()
            .any(|m| m.contains("tool executed successfully"))
    );
}

#[tokio::test]
async fn agent_handles_tool_blocked_error() {
    use zeph_llm::provider::{ChatResponse, ToolUseRequest};
    let tool_call = ToolUseRequest {
        id: "call1".into(),
        name: "bash".into(),
        input: serde_json::json!({}),
    };
    let (mock, _) = MockProvider::default().with_tool_use(vec![
        ChatResponse::ToolUse {
            text: None,
            tool_calls: vec![tool_call],
            thinking_blocks: vec![],
        },
        ChatResponse::Text("done".into()),
    ]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::new(vec![Err(ToolError::Blocked {
        command: "rm -rf /".to_string(),
    })]);

    let agent_channel = MockChannel::new(vec!["test".to_string()]);
    let sent = agent_channel.sent.clone();

    let mut agent = Agent::new(
        AnyProvider::Mock(mock),
        agent_channel,
        registry,
        None,
        5,
        executor,
    );

    let result = agent.run().await;
    assert!(result.is_ok());

    let sent_msgs = sent.lock().unwrap();
    assert!(
        sent_msgs
            .iter()
            .any(|m| m.contains("blocked by security policy"))
    );
}

#[tokio::test]
async fn agent_handles_tool_sandbox_violation() {
    use zeph_llm::provider::{ChatResponse, ToolUseRequest};
    let tool_call = ToolUseRequest {
        id: "call1".into(),
        name: "bash".into(),
        input: serde_json::json!({}),
    };
    let (mock, _) = MockProvider::default().with_tool_use(vec![
        ChatResponse::ToolUse {
            text: None,
            tool_calls: vec![tool_call],
            thinking_blocks: vec![],
        },
        ChatResponse::Text("done".into()),
    ]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::new(vec![Err(ToolError::SandboxViolation {
        path: "/etc/passwd".to_string(),
    })]);

    let agent_channel = MockChannel::new(vec!["test".to_string()]);
    let sent = agent_channel.sent.clone();

    let mut agent = Agent::new(
        AnyProvider::Mock(mock),
        agent_channel,
        registry,
        None,
        5,
        executor,
    );

    let result = agent.run().await;
    assert!(result.is_ok());

    let sent_msgs = sent.lock().unwrap();
    // In the native path, SandboxViolation is classified as PolicyBlocked and formatted
    // as a tool_result feedback string (not a direct user message). The ToolError display
    // string ("path not allowed by sandbox") appears in the tool output sent to the channel.
    assert!(
        sent_msgs
            .iter()
            .any(|m| m.contains("path not allowed by sandbox"))
    );
}

#[tokio::test]
async fn agent_handles_tool_confirmation_approved() {
    use zeph_llm::provider::{ChatResponse, ToolUseRequest};
    let tool_call = ToolUseRequest {
        id: "call1".into(),
        name: "bash".into(),
        input: serde_json::json!({}),
    };
    let (mock, _) = MockProvider::default().with_tool_use(vec![
        ChatResponse::ToolUse {
            text: None,
            tool_calls: vec![tool_call],
            thinking_blocks: vec![],
        },
        ChatResponse::Text("done".into()),
    ]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::new(vec![Err(ToolError::ConfirmationRequired {
        command: "dangerous command".to_string(),
    })]);

    let agent_channel = MockChannel::new(vec!["test".to_string()]).with_confirmations(vec![true]);
    let sent = agent_channel.sent.clone();

    let mut agent = Agent::new(
        AnyProvider::Mock(mock),
        agent_channel,
        registry,
        None,
        5,
        executor,
    );

    let result = agent.run().await;
    assert!(result.is_ok());

    let sent_msgs = sent.lock().unwrap();
    assert!(!sent_msgs.is_empty());
}

#[tokio::test]
async fn agent_handles_tool_confirmation_denied() {
    use zeph_llm::provider::{ChatResponse, ToolUseRequest};
    let tool_call = ToolUseRequest {
        id: "call1".into(),
        name: "bash".into(),
        input: serde_json::json!({}),
    };
    let (mock, _) = MockProvider::default().with_tool_use(vec![
        ChatResponse::ToolUse {
            text: None,
            tool_calls: vec![tool_call],
            thinking_blocks: vec![],
        },
        ChatResponse::Text("done".into()),
    ]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::new(vec![Err(ToolError::ConfirmationRequired {
        command: "dangerous command".to_string(),
    })]);

    let agent_channel = MockChannel::new(vec!["test".to_string()]).with_confirmations(vec![false]);
    let sent = agent_channel.sent.clone();

    let mut agent = Agent::new(
        AnyProvider::Mock(mock),
        agent_channel,
        registry,
        None,
        5,
        executor,
    );

    let result = agent.run().await;
    assert!(result.is_ok());

    let sent_msgs = sent.lock().unwrap();
    // In the native path, a denied confirmation results in a "[cancelled by user]" tool
    // output sent via send_tool_output (not a direct "Command cancelled" user message).
    assert!(sent_msgs.iter().any(|m| m.contains("[cancelled by user]")));
}

#[tokio::test]
async fn agent_handles_streaming_response() {
    let (mock, _) =
        MockProvider::default().with_tool_use(vec![zeph_llm::provider::ChatResponse::Text(
            "streaming response".to_string(),
        )]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let agent_channel = MockChannel::new(vec!["test".to_string()]);
    let sent = agent_channel.sent.clone();

    let mut agent = Agent::new(
        AnyProvider::Mock(mock),
        agent_channel,
        registry,
        None,
        5,
        executor,
    );

    let result = agent.run().await;
    assert!(result.is_ok());

    let sent_msgs = sent.lock().unwrap();
    assert!(!sent_msgs.is_empty());
}

#[tokio::test]
async fn agent_maybe_redact_enabled() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let security = SecurityConfig {
        redact_secrets: true,
        ..Default::default()
    };

    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_security(security, TimeoutConfig::default());

    let text = "token: sk-abc123secret";
    let redacted = agent.maybe_redact(text);
    assert_ne!(AsRef::<str>::as_ref(&redacted), text);
}

#[tokio::test]
async fn agent_maybe_redact_disabled() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let security = SecurityConfig {
        redact_secrets: false,
        ..Default::default()
    };

    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_security(security, TimeoutConfig::default());

    let text = "password=secret123";
    let redacted = agent.maybe_redact(text);
    assert_eq!(AsRef::<str>::as_ref(&redacted), text);
}

#[tokio::test]
async fn agent_handles_multiple_messages() {
    let provider = mock_provider(vec![
        "first response".to_string(),
        "second response".to_string(),
    ]);
    // Both messages arrive simultaneously via try_recv(), so they merge
    // within the 500ms window into a single "first\nsecond" message.
    let channel = MockChannel::new(vec!["first".to_string(), "second".to_string()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::new(vec![Ok(None), Ok(None)]);

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let result = agent.run().await;
    assert!(result.is_ok());
    assert_eq!(agent.msg.messages.len(), 3);
    assert_eq!(agent.msg.messages[1].content, "first\nsecond");
}

#[tokio::test]
async fn agent_handles_tool_output_with_error_marker() {
    let provider = mock_provider(vec!["response".to_string(), "retry".to_string()]);
    let channel = MockChannel::new(vec!["test".to_string()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::new(vec![
        Ok(Some(ToolOutput {
            tool_name: "bash".into(),
            summary: "[error] command failed [exit code 1]".to_string(),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: None,
        })),
        Ok(None),
    ]);

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let result = agent.run().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn agent_handles_empty_tool_output() {
    let provider = mock_provider(vec!["response".to_string()]);
    let channel = MockChannel::new(vec!["test".to_string()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::new(vec![Ok(Some(ToolOutput {
        tool_name: "bash".into(),
        summary: "   ".to_string(),
        blocks_executed: 1,
        filter_stats: None,
        diff: None,
        streamed: false,
        terminal_id: None,
        locations: None,
        raw_response: None,
        claim_source: None,
    }))]);

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let result = agent.run().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn shutdown_signal_helper_returns_on_true() {
    let (tx, rx) = watch::channel(false);
    let handle = tokio::spawn(async move {
        let mut rx_clone = rx;
        shutdown_signal(&mut rx_clone).await;
    });

    tx.send(true).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_millis(100), handle).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn recv_optional_returns_pending_when_no_receiver() {
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(10),
        recv_optional::<SkillEvent>(&mut None),
    )
    .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn recv_optional_receives_from_channel() {
    let (tx, rx) = mpsc::channel(1);
    tx.send(SkillEvent::Changed).await.unwrap();

    let result = recv_optional(&mut Some(rx)).await;
    assert!(result.is_some());
}

#[tokio::test]
async fn agent_with_skill_reload_sets_paths() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (_tx, rx) = mpsc::channel(1);

    let paths = vec![std::path::PathBuf::from("/test/path")];
    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_skill_reload(paths.clone(), rx);

    assert_eq!(agent.skill_state.skill_paths, paths);
}

#[tokio::test]
async fn agent_handles_tool_execution_error() {
    use zeph_llm::provider::{ChatResponse, ToolUseRequest};
    let tool_call = ToolUseRequest {
        id: "call1".into(),
        name: "bash".into(),
        input: serde_json::json!({}),
    };
    let (mock, _) = MockProvider::default().with_tool_use(vec![
        ChatResponse::ToolUse {
            text: None,
            tool_calls: vec![tool_call],
            thinking_blocks: vec![],
        },
        ChatResponse::Text("done".into()),
    ]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::new(vec![Err(ToolError::Timeout { timeout_secs: 30 })]);

    let agent_channel = MockChannel::new(vec!["test".to_string()]);
    let sent = agent_channel.sent.clone();

    let mut agent = Agent::new(
        AnyProvider::Mock(mock),
        agent_channel,
        registry,
        None,
        5,
        executor,
    );

    let result = agent.run().await;
    assert!(result.is_ok());

    let sent_msgs = sent.lock().unwrap();
    // In the native path, Timeout errors are formatted as tool_result feedback sent via
    // send_tool_output. The error string ("command timed out") appears in the tool output.
    assert!(
        sent_msgs
            .iter()
            .any(|m| m.contains("timed out") || m.contains("timeout"))
    );
}

#[tokio::test]
async fn agent_processes_multi_turn_tool_execution() {
    use zeph_llm::provider::{ChatResponse, ToolUseRequest};
    let tool_call = ToolUseRequest {
        id: "call1".into(),
        name: "bash".into(),
        input: serde_json::json!({}),
    };
    // Native path: LLM returns ToolUse first, then a final Text after tool results.
    let (mock, _) = MockProvider::default().with_tool_use(vec![
        ChatResponse::ToolUse {
            text: None,
            tool_calls: vec![tool_call],
            thinking_blocks: vec![],
        },
        ChatResponse::Text("step complete".into()),
    ]);
    let channel = MockChannel::new(vec!["start task".to_string()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::new(vec![Ok(Some(ToolOutput {
        tool_name: "bash".into(),
        summary: "step 1 complete".to_string(),
        blocks_executed: 1,
        filter_stats: None,
        diff: None,
        streamed: false,
        terminal_id: None,
        locations: None,
        raw_response: None,
        claim_source: None,
    }))]);

    let mut agent = Agent::new(
        AnyProvider::Mock(mock),
        channel,
        registry,
        None,
        5,
        executor,
    );

    let result = agent.run().await;
    assert!(result.is_ok());
    // Native path: user message + assistant tool_use + user tool_result + assistant response.
    assert!(agent.msg.messages.len() > 3);
}

#[tokio::test]
async fn agent_respects_max_shell_iterations() {
    let mut responses = vec![];
    for _ in 0..10 {
        responses.push("response".to_string());
    }
    let provider = mock_provider(responses);
    let channel = MockChannel::new(vec!["test".to_string()]);
    let registry = create_test_registry();

    let mut outputs = vec![];
    for _ in 0..10 {
        outputs.push(Ok(Some(ToolOutput {
            tool_name: "bash".into(),
            summary: "continuing".to_string(),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: None,
        })));
    }
    let executor = MockToolExecutor::new(outputs);

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let result = agent.run().await;
    assert!(result.is_ok());
    let assistant_count = agent
        .msg
        .messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .count();
    assert!(assistant_count <= 10);
}

#[tokio::test]
async fn agent_with_metrics_sets_initial_values() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.runtime.model_name = "test-model".to_string();
    let _agent = agent.with_metrics(tx);

    let snapshot = rx.borrow().clone();
    assert_eq!(snapshot.provider_name, "mock");
    assert_eq!(snapshot.model_name, "test-model");
    assert_eq!(snapshot.total_skills, 1);
    assert!(
        snapshot.prompt_tokens > 0,
        "initial prompt estimate should be non-zero"
    );
    assert_eq!(snapshot.total_tokens, snapshot.prompt_tokens);
    assert!(
        !snapshot.active_skills.is_empty(),
        "active_skills should be pre-populated at startup"
    );
}

#[tokio::test]
async fn skill_all_candidates_dropped_below_threshold_active_skills_empty() {
    // When min_injection_score = f32::MAX, every scored candidate fails the retain
    // gate. The agent must not panic and must report zero active skills for the turn.
    use zeph_skills::matcher::{SkillMatcher, SkillMatcherBackend};

    // Use an embedding-capable provider so SkillMatcher::new succeeds and
    // match_skills can embed the query — exercising the retain gate instead of
    // the empty-matcher fallback.
    let provider = AnyProvider::Mock(
        MockProvider::with_responses(vec!["response".to_string()]).with_embedding(vec![1.0, 0.0]),
    );
    let channel = MockChannel::new(vec!["hello".to_string()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

    // Build an in-memory matcher so the retain gate is exercised rather than
    // falling through the empty-matcher fallback path.
    let all_meta_owned: Vec<zeph_skills::loader::SkillMeta> = {
        let registry_guard = agent.skill_state.registry.read();
        registry_guard.all_meta().into_iter().cloned().collect()
    };
    let embed_fn = |_text: &str| -> zeph_skills::matcher::EmbedFuture {
        Box::pin(async { Ok(vec![1.0_f32, 0.0]) })
    };
    let matcher = SkillMatcher::new(&all_meta_owned.iter().collect::<Vec<_>>(), embed_fn).await;
    agent.skill_state.matcher = matcher.map(SkillMatcherBackend::InMemory);
    // Set an impossibly high threshold so every candidate is dropped.
    agent.skill_state.min_injection_score = f32::MAX;

    agent.run().await.unwrap();

    let snapshot = rx.borrow().clone();
    assert!(
        snapshot.active_skills.is_empty(),
        "no skills should be active when all candidates fail the score gate"
    );
}

#[tokio::test]
async fn agent_metrics_update_on_llm_call() {
    let provider = mock_provider(vec!["response".to_string()]);
    let channel = MockChannel::new(vec!["hello".to_string()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

    agent.run().await.unwrap();

    let snapshot = rx.borrow().clone();
    assert_eq!(snapshot.api_calls, 1);
    assert!(snapshot.total_tokens > 0);
}

#[tokio::test]
async fn agent_metrics_streaming_updates_completion_tokens() {
    let provider = mock_provider_streaming(vec!["streaming response".to_string()]);
    let channel = MockChannel::new(vec!["test".to_string()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

    agent.run().await.unwrap();

    let snapshot = rx.borrow().clone();
    assert!(snapshot.completion_tokens > 0);
    assert_eq!(snapshot.api_calls, 1);
}

#[tokio::test]
async fn agent_metrics_persist_increments_count() {
    let provider = mock_provider(vec!["response".to_string()]);
    let channel = MockChannel::new(vec!["hello".to_string()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

    agent.run().await.unwrap();

    let snapshot = rx.borrow().clone();
    assert!(snapshot.sqlite_message_count == 0, "no memory = no persist");
}

#[tokio::test]
async fn agent_metrics_skills_updated_on_prompt_rebuild() {
    let provider = mock_provider(vec!["response".to_string()]);
    let channel = MockChannel::new(vec!["hello".to_string()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

    agent.run().await.unwrap();

    let snapshot = rx.borrow().clone();
    assert_eq!(snapshot.total_skills, 1);
    assert!(!snapshot.active_skills.is_empty());
}
