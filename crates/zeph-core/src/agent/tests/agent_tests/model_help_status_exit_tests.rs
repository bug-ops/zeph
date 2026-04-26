// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[allow(unused_imports)]
use super::*;

#[test]
fn set_model_updates_model_name() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    assert!(agent.set_model("claude-opus-4-6").is_ok());
    assert_eq!(agent.runtime.model_name, "claude-opus-4-6");
}

#[test]
fn set_model_overwrites_previous_value() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.set_model("model-a").unwrap();
    agent.set_model("model-b").unwrap();
    assert_eq!(agent.runtime.model_name, "model-b");
}

#[tokio::test]
async fn model_command_switch_sends_confirmation() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let out = agent
        .handle_model_command_as_string("/model my-new-model")
        .await;
    assert!(
        out.contains("my-new-model"),
        "expected switch confirmation, got: {out}"
    );
}

#[tokio::test]
async fn model_command_list_no_cache_fetches_remote() {
    // With mock provider, list_models_remote returns empty vec — agent sends "No models".
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    // Ensure cache is stale for mock provider slug
    zeph_llm::model_cache::ModelCache::for_slug("mock").invalidate();
    let out = agent.handle_model_command_as_string("/model").await;
    // Mock returns empty list → "No models available."
    assert!(
        out.contains("No models"),
        "expected empty model list message, got: {out}"
    );
}

#[tokio::test]
async fn model_command_refresh_sends_result() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let out = agent.handle_model_command_as_string("/model refresh").await;
    assert!(
        out.contains("Fetched"),
        "expected fetch confirmation, got: {out}"
    );
}

#[tokio::test]
async fn model_command_valid_model_accepted() {
    // Ensure cache is stale so the handler falls back to list_models_remote().
    zeph_llm::model_cache::ModelCache::for_slug("mock").invalidate();

    let models = vec![
        zeph_llm::model_cache::RemoteModelInfo {
            id: "llama3:8b".to_string(),
            display_name: "Llama 3 8B".to_string(),
            context_window: Some(8192),
            created_at: None,
        },
        zeph_llm::model_cache::RemoteModelInfo {
            id: "qwen3:8b".to_string(),
            display_name: "Qwen3 8B".to_string(),
            context_window: Some(32768),
            created_at: None,
        },
    ];
    let provider = mock_provider_with_models(vec![], models);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let out = agent
        .handle_model_command_as_string("/model llama3:8b")
        .await;

    assert!(
        out.contains("Switched to model: llama3:8b"),
        "expected switch confirmation, got: {out}"
    );
    assert!(
        !out.contains("Unknown model"),
        "unexpected rejection for valid model, got: {out}"
    );
}

#[tokio::test]
async fn model_command_invalid_model_rejected() {
    // Ensure cache is stale so the handler falls back to list_models_remote().
    zeph_llm::model_cache::ModelCache::for_slug("mock").invalidate();

    let models = vec![zeph_llm::model_cache::RemoteModelInfo {
        id: "qwen3:8b".to_string(),
        display_name: "Qwen3 8B".to_string(),
        context_window: None,
        created_at: None,
    }];
    let provider = mock_provider_with_models(vec![], models);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let out = agent
        .handle_model_command_as_string("/model nonexistent-model")
        .await;

    assert!(
        out.contains("Unknown model") && out.contains("nonexistent-model"),
        "expected rejection with model name, got: {out}"
    );
    assert!(
        out.contains("qwen3:8b"),
        "expected available models list, got: {out}"
    );
    assert!(
        !out.contains("Switched to model"),
        "should not switch to invalid model, got: {out}"
    );
}

#[tokio::test]
async fn model_command_empty_model_list_warns_and_proceeds() {
    // Ensure cache is stale so the handler falls back to list_models_remote().
    // MockProvider returns empty vec → warning shown, switch proceeds.
    zeph_llm::model_cache::ModelCache::for_slug("mock").invalidate();

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let out = agent
        .handle_model_command_as_string("/model unknown-model")
        .await;

    assert!(
        out.contains("unavailable"),
        "expected warning about unavailable model list, got: {out}"
    );
    assert!(
        out.contains("Switched to model: unknown-model"),
        "expected switch to proceed despite missing model list, got: {out}"
    );
}

#[tokio::test]
async fn help_command_lists_commands() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec!["/help".to_string()]);
    let sent = channel.sent.clone();
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent.run().await;
    assert!(result.is_ok());

    let messages = sent.lock().unwrap();
    assert!(!messages.is_empty(), "expected /help output");
    let output = messages.join("\n");
    assert!(output.contains("/help"), "expected /help in output");
    assert!(output.contains("/exit"), "expected /exit in output");
    assert!(output.contains("/status"), "expected /status in output");
    assert!(output.contains("/skills"), "expected /skills in output");
    assert!(output.contains("/model"), "expected /model in output");
}

#[tokio::test]
async fn help_command_does_not_include_unknown_commands() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec!["/help".to_string()]);
    let sent = channel.sent.clone();
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent.run().await;
    assert!(result.is_ok());

    let messages = sent.lock().unwrap();
    let output = messages.join("\n");
    // /ingest does not exist in the codebase — must not appear
    assert!(
        !output.contains("/ingest"),
        "unexpected /ingest in /help output"
    );
}

#[tokio::test]
async fn status_command_includes_provider_and_model() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec!["/status".to_string()]);
    let sent = channel.sent.clone();
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent.run().await;
    assert!(result.is_ok());

    let messages = sent.lock().unwrap();
    assert!(!messages.is_empty(), "expected /status output");
    let output = messages.join("\n");
    assert!(output.contains("Provider:"), "expected Provider: field");
    assert!(output.contains("Model:"), "expected Model: field");
    assert!(output.contains("Uptime:"), "expected Uptime: field");
    assert!(output.contains("Tokens:"), "expected Tokens: field");
}

// Regression test for #1415: MetricsCollector must be wired in CLI mode (no TUI).
// Before the fix, metrics_tx was None in non-TUI mode so /status always showed zeros.
#[tokio::test]
async fn status_command_shows_metrics_in_cli_mode() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec!["/status".to_string()]);
    let sent = channel.sent.clone();
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let (tx, _rx) = watch::channel(MetricsSnapshot::default());
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

    // Simulate metrics that would be populated by a real LLM call.
    agent.update_metrics(|m| {
        m.api_calls = 3;
        m.prompt_tokens = 100;
        m.completion_tokens = 50;
    });

    let result = agent.run().await;
    assert!(result.is_ok());

    let messages = sent.lock().unwrap();
    let output = messages.join("\n");
    assert!(
        output.contains("API calls: 3"),
        "expected non-zero api_calls in /status output; got: {output}"
    );
    assert!(
        output.contains("100 prompt / 50 completion"),
        "expected non-zero tokens in /status output; got: {output}"
    );
}

#[tokio::test]
async fn status_command_shows_orchestration_stats_when_plans_nonzero() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec!["/status".to_string()]);
    let sent = channel.sent.clone();
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let (tx, _rx) = watch::channel(MetricsSnapshot::default());
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

    agent.update_metrics(|m| {
        m.orchestration.plans_total = 2;
        m.orchestration.tasks_total = 10;
        m.orchestration.tasks_completed = 8;
        m.orchestration.tasks_failed = 1;
        m.orchestration.tasks_skipped = 1;
    });

    let result = agent.run().await;
    assert!(result.is_ok());

    let messages = sent.lock().unwrap();
    let output = messages.join("\n");
    assert!(
        output.contains("Orchestration:"),
        "expected Orchestration: section; got: {output}"
    );
    assert!(
        output.contains("Plans:     2"),
        "expected Plans: 2; got: {output}"
    );
    assert!(
        output.contains("8/10 completed"),
        "expected 8/10 completed; got: {output}"
    );
    assert!(
        output.contains("Failed:    1"),
        "expected Failed: 1; got: {output}"
    );
    assert!(
        output.contains("Skipped:   1"),
        "expected Skipped: 1; got: {output}"
    );
}

#[tokio::test]
async fn status_command_hides_orchestration_when_no_plans() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec!["/status".to_string()]);
    let sent = channel.sent.clone();
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let (tx, _rx) = watch::channel(MetricsSnapshot::default());
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);
    // No orchestration metrics set — plans_total stays 0.

    let result = agent.run().await;
    assert!(result.is_ok());

    let messages = sent.lock().unwrap();
    let output = messages.join("\n");
    assert!(
        !output.contains("Orchestration:"),
        "Orchestration: section must be absent when no plans ran; got: {output}"
    );
}

#[tokio::test]
async fn exit_command_breaks_run_loop() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec!["/exit".to_string()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent.run().await;
    assert!(result.is_ok());
    // /exit should not produce any LLM message — only system message in history
    assert_eq!(agent.msg.messages.len(), 1, "expected only system message");
}

#[tokio::test]
async fn quit_command_breaks_run_loop() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec!["/quit".to_string()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent.run().await;
    assert!(result.is_ok());
    assert_eq!(agent.msg.messages.len(), 1, "expected only system message");
}

#[tokio::test]
async fn exit_command_sends_info_and_continues_when_not_supported() {
    let provider = mock_provider(vec![]);
    // Channel that does not support exit: /exit should NOT break the loop,
    // it should send an info message and then yield the next message.
    let channel = MockChannel::new(vec![
        "/exit".to_string(),
        // second message is empty → causes recv() to return None → loop exits naturally
    ])
    .without_exit_support();
    let sent = channel.sent.clone();
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent.run().await;
    assert!(result.is_ok());

    let messages = sent.lock().unwrap();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("/exit is not supported")),
        "expected info message, got: {messages:?}"
    );
}

#[test]
fn slash_commands_registry_has_no_ingest() {
    use zeph_commands::COMMANDS;
    assert!(
        !COMMANDS.iter().any(|c| c.name == "/ingest"),
        "/ingest is not implemented and must not appear in COMMANDS"
    );
}

#[test]
fn slash_commands_graph_and_plan_have_no_feature_gate() {
    use zeph_commands::COMMANDS;
    for cmd in COMMANDS {
        if cmd.name == "/graph" || cmd.name == "/plan" {
            assert!(
                cmd.feature_gate.is_none(),
                "{} should have feature_gate: None",
                cmd.name
            );
        }
    }
}

// Regression tests for issue #1418: bare slash commands must not fall through to LLM.

#[tokio::test]
async fn bare_skill_command_does_not_invoke_llm() {
    // Provider has no responses — if LLM is called the agent would receive an empty response
    // and send "empty response" to the channel. The handler should return before reaching LLM.
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec!["/skill".to_string()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let result = agent.run().await;
    assert!(result.is_ok());

    let sent = agent.channel.sent_messages();
    // Handler sends the "Unknown /skill subcommand" usage message — not an LLM response.
    assert!(
        sent.iter().any(|m| m.contains("Unknown /skill subcommand")),
        "bare /skill must send usage; got: {sent:?}"
    );
    // No assistant message should be added to history (LLM was not called).
    assert!(
        agent.msg.messages.iter().all(|m| m.role != Role::Assistant),
        "bare /skill must not produce an assistant message; messages: {:?}",
        agent.msg.messages
    );
}

#[tokio::test]
async fn bare_feedback_command_does_not_invoke_llm() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec!["/feedback".to_string()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let result = agent.run().await;
    assert!(result.is_ok());

    let sent = agent.channel.sent_messages();
    assert!(
        sent.iter().any(|m| m.contains("Usage: /feedback")),
        "bare /feedback must send usage; got: {sent:?}"
    );
    assert!(
        agent.msg.messages.iter().all(|m| m.role != Role::Assistant),
        "bare /feedback must not produce an assistant message; messages: {:?}",
        agent.msg.messages
    );
}

#[tokio::test]
async fn bare_image_command_sends_usage() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec!["/image".to_string()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let result = agent.run().await;
    assert!(result.is_ok());

    let sent = agent.channel.sent_messages();
    assert!(
        sent.iter().any(|m| m.contains("Usage: /image <path>")),
        "bare /image must send usage; got: {sent:?}"
    );
    assert!(
        agent.msg.messages.iter().all(|m| m.role != Role::Assistant),
        "bare /image must not produce an assistant message; messages: {:?}",
        agent.msg.messages
    );
}

#[tokio::test]
async fn feedback_positive_records_user_approval() {
    let provider = mock_provider(vec![]);
    let memory = SemanticMemory::new(":memory:", "http://127.0.0.1:1", provider.clone(), "test")
        .await
        .unwrap();
    let cid = memory.sqlite().create_conversation().await.unwrap();
    let memory = std::sync::Arc::new(memory);

    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
        memory.clone(),
        cid,
        50,
        5,
        50,
    );

    agent
        .handle_feedback_as_string("git great job, works perfectly")
        .await
        .unwrap();

    let row: Option<(String,)> =
        sqlx::query_as("SELECT outcome FROM skill_outcomes WHERE skill_name = 'git' LIMIT 1")
            .fetch_optional(memory.sqlite().pool())
            .await
            .unwrap();
    assert_eq!(
        row.map(|r| r.0).as_deref(),
        Some("user_approval"),
        "positive feedback must be recorded as user_approval"
    );
}

#[tokio::test]
async fn feedback_negative_records_user_rejection() {
    let provider = mock_provider(vec![]);
    let memory = SemanticMemory::new(":memory:", "http://127.0.0.1:1", provider.clone(), "test")
        .await
        .unwrap();
    let cid = memory.sqlite().create_conversation().await.unwrap();
    let memory = std::sync::Arc::new(memory);

    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
        memory.clone(),
        cid,
        50,
        5,
        50,
    );

    agent
        .handle_feedback_as_string("git that was wrong, bad output")
        .await
        .unwrap();

    let row: Option<(String,)> =
        sqlx::query_as("SELECT outcome FROM skill_outcomes WHERE skill_name = 'git' LIMIT 1")
            .fetch_optional(memory.sqlite().pool())
            .await
            .unwrap();
    assert_eq!(
        row.map(|r| r.0).as_deref(),
        Some("user_rejection"),
        "negative feedback must be recorded as user_rejection"
    );
}

#[tokio::test]
async fn feedback_neutral_records_user_approval() {
    let provider = mock_provider(vec![]);
    let memory = SemanticMemory::new(":memory:", "http://127.0.0.1:1", provider.clone(), "test")
        .await
        .unwrap();
    let cid = memory.sqlite().create_conversation().await.unwrap();
    let memory = std::sync::Arc::new(memory);

    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
        memory.clone(),
        cid,
        50,
        5,
        50,
    );

    // Ambiguous/neutral feedback — FeedbackDetector returns None → user_approval
    agent.handle_feedback_as_string("git ok").await.unwrap();

    let row: Option<(String,)> =
        sqlx::query_as("SELECT outcome FROM skill_outcomes WHERE skill_name = 'git' LIMIT 1")
            .fetch_optional(memory.sqlite().pool())
            .await
            .unwrap();
    assert_eq!(
        row.map(|r| r.0).as_deref(),
        Some("user_approval"),
        "neutral/ambiguous feedback must be recorded as user_approval"
    );
}
