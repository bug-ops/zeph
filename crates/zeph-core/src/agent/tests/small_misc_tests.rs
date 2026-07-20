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

#[test]
fn auto_budget_true_budget_zero_provider_window_zero_falls_back_to_128k() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.memory.auto_budget = true;
    config.memory.context_budget_tokens = 0;
    // Provider reports Some(0) — a misconfigured window, not "unknown" (None).
    // Must still fall back to 128k rather than resolving to a real 0-token budget.
    let provider = AnyProvider::Mock(MockProvider::default().with_context_window(0));
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
    // Delegation gate (spec 042, issue #5857): a fresh default config has `agents.enabled =
    // false`, which now resolves to `Disabled` and would short-circuit before ever reaching
    // the "callback missing" branch this test exercises. Opt in so the pre-existing behavior
    // under test is reachable.
    h.agent.services.orchestration.subagent_config.enabled = true;
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
    h.agent.services.orchestration.subagent_config.enabled = true;
    h.agent.runtime.config.acp_subagent_spawn_fn = Some(std::sync::Arc::new(|cmd: String| {
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

/// Spec 042 FR-003 (issue #5857): `delegation_mode = "disabled"` (or, as here, the `enabled`
/// outer kill switch left at its default `false`) must reject the ACP `/subagent spawn` path
/// too — even though it never touches `SubAgentManager`/`SpawnContext` at all (critic's
/// corrected finding; see `handle_subagent_slash`'s doc comment). A configured callback must
/// never be invoked while disabled.
#[tokio::test]
async fn subagent_spawn_disabled_by_delegation_mode_returns_disabled_message() {
    let mut h = QuickTestAgent::minimal("");
    // Default config: `agents.enabled = false` → effective mode `Disabled`.
    h.agent.runtime.config.acp_subagent_spawn_fn = Some(std::sync::Arc::new(|cmd: String| {
        Box::pin(async move { Ok(format!("spawned: {cmd}")) })
    }));
    let result = h
        .agent
        .dispatch_slash_command("/subagent spawn my-command")
        .await;
    assert!(result.is_some(), "must be intercepted");
    let output = h.sent_messages().join("\n");
    assert!(
        output.to_lowercase().contains("disabled"),
        "expected a disabled-by-configuration message, got: {output}"
    );
    assert!(
        !output.contains("spawned: my-command"),
        "callback must never run while delegation is disabled, got: {output}"
    );
}

/// `delegation_mode = "explicit_request_only"` with `enabled = true` must still permit
/// `/subagent spawn` — it is itself an explicit user action (spec 042, issue #5857).
#[tokio::test]
async fn subagent_spawn_explicit_request_only_still_allows_acp_spawn() {
    let mut h = QuickTestAgent::minimal("");
    h.agent.services.orchestration.subagent_config.enabled = true;
    h.agent
        .services
        .orchestration
        .subagent_config
        .delegation_mode = zeph_config::DelegationMode::ExplicitRequestOnly;
    h.agent.runtime.config.acp_subagent_spawn_fn = Some(std::sync::Arc::new(|cmd: String| {
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
        "expected callback output under explicit_request_only, got: {output}"
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
