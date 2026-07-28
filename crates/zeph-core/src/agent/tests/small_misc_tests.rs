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

/// Issue #6545, N1 (revised — see security audit `2026-07-28T00-38-48-security.md`):
/// `AcpSubagentSpawnFn` is fallible, but its `Err` arm cannot be assumed to mean "never
/// launched" — the real callback (`zeph_acp::run_session`) launches the child process before
/// it can time out or fail post-launch, and `Result<String, String>` erases that distinction
/// by the time it crosses this boundary. So a *simulated post-launch failure* (as opposed to
/// the callback simply not existing, or the pre-flight budget `check()` rejecting the attempt
/// before `spawn_fn` is ever called) must still consume budget: undercounting a real process
/// launch is the actual vulnerability this cap exists to prevent, not the minor overcount of
/// the rarer "never launched" sub-case. Regression guard against reverting to the earlier,
/// incorrect `Ok`-arm-only commit point.
#[tokio::test]
async fn subagent_spawn_post_launch_failure_still_consumes_budget() {
    let mut h = QuickTestAgent::minimal("");
    h.agent.services.orchestration.subagent_config.enabled = true;
    h.agent.runtime.config.acp_subagent_spawn_fn = Some(std::sync::Arc::new(|_cmd: String| {
        Box::pin(async move { Err("launch failed".to_owned()) })
    }));

    let result = h
        .agent
        .dispatch_slash_command("/subagent spawn my-command")
        .await;
    assert!(result.is_some(), "must be intercepted");
    let output = h.sent_messages().join("\n");
    assert!(
        output.contains("launch failed"),
        "expected the spawn_fn error, got: {output}"
    );
    assert_eq!(
        h.agent
            .services
            .orchestration
            .session_spawn_budget
            .spawned(),
        1,
        "spawn_fn having returned at all — even Err — means it was invoked and must consume \
         budget, since its Err arm can mean a real child process ran and then failed"
    );
}

/// Issue #6545, N3(b): with no `SubAgentManager` wired at all (the `QuickTestAgent::minimal`
/// harness default — matches serve/daemon/acp bootstrap paths per critic finding N2/R4), the
/// ACP chokepoint must still enforce the cap via `OrchestrationState`'s own fallback budget.
#[tokio::test]
async fn subagent_spawn_cap_reached_enforced_without_manager() {
    let mut h = QuickTestAgent::minimal("");
    h.agent.services.orchestration.subagent_config.enabled = true;
    h.agent
        .services
        .orchestration
        .subagent_config
        .max_spawns_per_session = 1;
    h.agent.runtime.config.acp_subagent_spawn_fn = Some(std::sync::Arc::new(|cmd: String| {
        Box::pin(async move { Ok(format!("spawned: {cmd}")) })
    }));
    assert!(
        h.agent.services.orchestration.subagent_manager.is_none(),
        "precondition: this test exercises the no-manager fallback path"
    );

    let first = h
        .agent
        .dispatch_slash_command("/subagent spawn first-command")
        .await;
    assert!(first.is_some());
    assert!(
        h.sent_messages()
            .join("\n")
            .contains("spawned: first-command")
    );

    let second = h
        .agent
        .dispatch_slash_command("/subagent spawn second-command")
        .await;
    assert!(second.is_some());
    let output = h.sent_messages().join("\n");
    assert!(
        !output.contains("spawned: second-command"),
        "cap must reject the second spawn, got: {output}"
    );
    assert!(
        output.contains("session spawn limit"),
        "rejection must name the session spawn limit, got: {output}"
    );
}

/// Issue #6545, N3(a): with a `SubAgentManager` wired (mirrors `runner.rs`'s production
/// `with_orchestration` path), an ACP spawn through the chokepoint must contribute to the
/// exact same cumulative count the manager itself sees — proving `Agent::session_budget`'s
/// accessor resolves to the manager's own budget, not a disconnected fallback, whenever a
/// manager exists. (The reverse — a manager-side spawn being observable through the ACP
/// chokepoint — is trivially true through the shared `&SessionSpawnBudget` reference and isn't
/// separately asserted here.)
#[tokio::test]
async fn subagent_spawn_visible_to_manager_budget_when_wired() {
    let mut h = QuickTestAgent::minimal("");
    h.agent.services.orchestration.subagent_config.enabled = true;
    h.agent.services.orchestration.subagent_manager = Some(zeph_subagent::SubAgentManager::new(4));
    h.agent.runtime.config.acp_subagent_spawn_fn = Some(std::sync::Arc::new(|cmd: String| {
        Box::pin(async move { Ok(format!("spawned: {cmd}")) })
    }));

    let result = h
        .agent
        .dispatch_slash_command("/subagent spawn my-command")
        .await;
    assert!(result.is_some());
    assert!(h.sent_messages().join("\n").contains("spawned: my-command"));

    let mgr = h
        .agent
        .services
        .orchestration
        .subagent_manager
        .as_ref()
        .expect("manager must still be wired");
    assert_eq!(
        mgr.session_budget().spawned(),
        1,
        "the ACP spawn must be visible through the manager's own budget handle"
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
