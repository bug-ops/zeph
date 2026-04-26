// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use crate::agent::agent_tests::QuickTestAgent;
use crate::agent::resolve_context_budget;
use crate::config::Config;
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;

#[test]
fn explicit_budget_returned_as_is() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.memory.auto_budget = false;
    config.memory.context_budget_tokens = 65536;
    let provider = AnyProvider::Mock(MockProvider::default());
    assert_eq!(resolve_context_budget(&config, &provider), 65536);
}

#[test]
fn auto_budget_true_budget_zero_no_window_falls_back_to_128k() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.memory.auto_budget = true;
    config.memory.context_budget_tokens = 0;
    // MockProvider::context_window() returns None — triggers 128k fallback.
    let provider = AnyProvider::Mock(MockProvider::default());
    assert_ne!(
        resolve_context_budget(&config, &provider),
        0,
        "budget must not be zero when auto_budget=true and context_budget_tokens=0"
    );
    assert_eq!(resolve_context_budget(&config, &provider), 128_000);
}

#[test]
fn auto_budget_false_budget_zero_falls_back_to_128k() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.memory.auto_budget = false;
    config.memory.context_budget_tokens = 0;
    let provider = AnyProvider::Mock(MockProvider::default());
    assert_eq!(resolve_context_budget(&config, &provider), 128_000);
}

#[tokio::test]
async fn subagent_no_args_returns_usage() {
    let mut h = QuickTestAgent::minimal("");
    let result = h.agent.dispatch_slash_command("/subagent").await;
    assert!(result.is_some(), "/subagent must be intercepted");
    let output = h.sent_messages().join("\n");
    assert!(
        output.contains("Usage"),
        "expected usage hint, got: {output}"
    );
}

#[tokio::test]
async fn subagent_spawn_no_command_returns_usage() {
    let mut h = QuickTestAgent::minimal("");
    let result = h.agent.dispatch_slash_command("/subagent spawn").await;
    assert!(result.is_some(), "/subagent spawn must be intercepted");
    let output = h.sent_messages().join("\n");
    assert!(
        output.contains("Usage"),
        "expected usage hint, got: {output}"
    );
}

#[tokio::test]
async fn subagent_spawn_without_callback_returns_not_available() {
    let mut h = QuickTestAgent::minimal("");
    let result = h
        .agent
        .dispatch_slash_command("/subagent spawn cargo run -- --acp")
        .await;
    assert!(result.is_some(), "must be intercepted");
    let output = h.sent_messages().join("\n");
    assert!(
        output.to_lowercase().contains("not available"),
        "expected 'not available' message, got: {output}"
    );
}

#[tokio::test]
async fn subagent_spawn_with_callback_returns_output() {
    let mut h = QuickTestAgent::minimal("");
    h.agent.runtime.acp_subagent_spawn_fn = Some(std::sync::Arc::new(|cmd: String| {
        Box::pin(async move { Ok(format!("spawned: {cmd}")) })
    }));
    let result = h
        .agent
        .dispatch_slash_command("/subagent spawn my-command")
        .await;
    assert!(result.is_some(), "must be intercepted");
    let output = h.sent_messages().join("\n");
    assert!(
        output.contains("spawned: my-command"),
        "expected callback output, got: {output}"
    );
}

#[tokio::test]
async fn subagent_unknown_subcommand_returns_error() {
    let mut h = QuickTestAgent::minimal("");
    let result = h.agent.dispatch_slash_command("/subagent badcmd").await;
    assert!(result.is_some(), "must be intercepted");
    let output = h.sent_messages().join("\n");
    assert!(
        output.contains("badcmd"),
        "error must name the subcommand, got: {output}"
    );
}

#[test]
fn layer_denial_carries_custom_reason() {
    use crate::runtime_layer::LayerDenial;

    let denial = LayerDenial {
        result: Ok(None),
        reason: "utility gate: score below threshold".to_owned(),
    };
    assert_eq!(denial.reason, "utility gate: score below threshold");
}
