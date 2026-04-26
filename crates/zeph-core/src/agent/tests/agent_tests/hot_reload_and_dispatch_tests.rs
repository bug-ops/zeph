// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[allow(unused_imports)]
use super::*;

// --- ShellExecutor hot-reload integration test (S1) ---

/// Verify that `warn_on_shell_overlay_divergence` rebuilds the live `ShellExecutor`
/// policy via `shell_policy_handle` when `blocked_commands` changes on hot-reload.
///
/// Exercises the code path at `agent/mod.rs`: `blocked_changed &&
/// shell_policy_handle.is_some() → h.rebuild(config)`.
#[test]
fn hot_reload_rebuilds_shell_blocklist() {
    use crate::config::Config;
    use zeph_config::tools::ShellConfig;

    // ShellExecutor with network allowed (no NETWORK_COMMANDS auto-added to blocklist).
    let base_cfg = ShellConfig {
        allow_network: true,
        blocked_commands: Vec::new(),
        ..ShellConfig::default()
    };
    let executor = zeph_tools::ShellExecutor::new(&base_cfg);
    let handle = executor.policy_handle();

    // "ping" must not appear in the initial blocklist.
    assert!(!handle.snapshot_blocked().contains(&"ping".to_owned()));

    // Wire the handle into a minimal agent's lifecycle.
    let harness = QuickTestAgent::minimal("ok");
    let mut agent = harness.agent;
    agent.lifecycle.shell_policy_handle = Some(handle.clone());
    agent.lifecycle.startup_shell_overlay = crate::ShellOverlaySnapshot {
        blocked: Vec::new(),
        allowed: Vec::new(),
    };

    // Simulate a hot-reload config that adds "ping" to blocked_commands.
    let mut new_config = Config::load(std::path::Path::new("/nonexistent")).unwrap();
    new_config.tools.shell.blocked_commands = vec!["ping".to_owned()];
    new_config.tools.shell.allow_network = true;

    let empty_overlay = zeph_plugins::ResolvedOverlay::default();
    agent.warn_on_shell_overlay_divergence(&empty_overlay, &new_config);

    // The handle (shared with the executor) must now contain "ping".
    assert!(
        handle.snapshot_blocked().contains(&"ping".to_owned()),
        "blocked_commands must be rebuilt live via shell_policy_handle"
    );
}

#[tokio::test]
async fn slash_command_error_is_non_fatal_session_registry() {
    // "/test-error" is registered only in test builds into the session/debug registry arm.
    // Before the fix this arm returned Err(AgentError::Other), terminating the agent.
    // After the fix the error is sent to the channel and the loop continues; the channel
    // then reaches EOF and the agent exits cleanly with Ok(()).
    //
    // Single message only — avoids MESSAGE_MERGE_WINDOW combining two rapid try_recv calls.
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec!["/test-error".to_string()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent.run().await;

    assert!(
        result.is_ok(),
        "agent must not exit with Err after CommandError: {result:?}"
    );
    let sent = agent.channel.sent_messages();
    assert!(
        sent.iter().any(|m| m.contains("boom")),
        "channel must receive the error message; got: {sent:?}"
    );
}

#[tokio::test]
async fn slash_command_error_is_non_fatal_agent_registry() {
    // "/loop every 2s tick" triggers CommandError from LoopCommand (minimum interval is 5s).
    // Before the fix this arm returned Err(AgentError::Other). After the fix the error is
    // surfaced to the channel and the loop continues; EOF then causes a clean exit.
    //
    // Single message only — avoids MESSAGE_MERGE_WINDOW combining two rapid try_recv calls.
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec!["/loop every 2s tick".to_string()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent.run().await;

    assert!(
        result.is_ok(),
        "agent must not exit with Err after CommandError: {result:?}"
    );
    let sent = agent.channel.sent_messages();
    assert!(
        !sent.is_empty(),
        "channel must receive the error message; got: {sent:?}"
    );
}

/// `/plugins list` is registered in the agent-command registry (fix for #3215).
/// The command must be routed — agent exits cleanly and the channel receives a reply.
#[tokio::test]
async fn plugins_list_is_routed_via_agent_registry() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec!["/plugins list".to_string()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent.run().await;

    assert!(result.is_ok(), "agent must exit cleanly: {result:?}");
    // PluginsCommand responds with either an installed-plugins listing or
    // "No plugins installed." — either way the channel must have received something.
    let sent = agent.channel.sent_messages();
    assert!(
        !sent.is_empty(),
        "/plugins list must produce output; got: {sent:?}"
    );
}
