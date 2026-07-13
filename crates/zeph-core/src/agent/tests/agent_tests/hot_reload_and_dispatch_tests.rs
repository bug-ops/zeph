// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
// Integration tests await full agent sessions; future size reflects real agent state.
#![allow(clippy::large_futures)]

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
    agent.runtime.lifecycle.shell_policy_handle = Some(handle.clone());
    agent.runtime.lifecycle.startup_shell_overlay = crate::ShellOverlaySnapshot {
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

// --- reload_skills() spawn_blocking swap-lock test (#5421) ---

/// `reload_skills()` builds the new registry off-lock via `spawn_blocking` and swaps
/// it in; verify the end-to-end effect is a correctly reloaded registry (new skill
/// visible in `all_meta()`) and a system prompt rebuilt from it.
#[tokio::test]
async fn reload_skills_offloads_registry_load_and_reflects_new_skills() {
    let harness = QuickTestAgent::minimal("ok");
    let mut agent = harness.agent;

    let temp_dir = tempfile::tempdir().unwrap();
    let skill_dir = temp_dir.path().join("hot-reload-skill");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: hot-reload-skill\ndescription: A skill added via hot reload\n---\nSkill body",
    )
    .unwrap();
    agent.services.skill.skill_paths = vec![temp_dir.path().to_path_buf()];

    agent.reload_skills().await;

    let names: Vec<String> = agent
        .services
        .skill
        .registry
        .read()
        .all_meta()
        .iter()
        .map(|m| m.name.clone())
        .collect();
    assert_eq!(
        names,
        vec!["hot-reload-skill".to_string()],
        "registry must reflect the reloaded skill set, not the original test-skill"
    );

    let system_prompt = &agent.msg.messages.first().unwrap().content;
    assert!(
        system_prompt.contains("hot-reload-skill"),
        "system prompt must be rebuilt from the reloaded registry; got: {system_prompt}"
    );
}

/// #6031: `reload_skills()` must be a no-op when `runtime.config.safe_mode` is set — the
/// single DRY choke point covering every entry point (runner/daemon/acp/serve) at once, so a
/// session that correctly started with an empty registry does not silently re-populate it
/// from disk on the first skill-file change.
#[tokio::test]
async fn reload_skills_is_noop_when_safe_mode_active() {
    let harness = QuickTestAgent::minimal("ok");
    let mut agent = harness.agent;
    agent.runtime.config.safe_mode = true;

    let names_before: Vec<String> = agent
        .services
        .skill
        .registry
        .read()
        .all_meta()
        .iter()
        .map(|m| m.name.clone())
        .collect();

    let temp_dir = tempfile::tempdir().unwrap();
    let skill_dir = temp_dir.path().join("hot-reload-skill");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: hot-reload-skill\ndescription: A skill added via hot reload\n---\nSkill body",
    )
    .unwrap();
    agent.services.skill.skill_paths = vec![temp_dir.path().to_path_buf()];

    agent.reload_skills().await;

    let names_after: Vec<String> = agent
        .services
        .skill
        .registry
        .read()
        .all_meta()
        .iter()
        .map(|m| m.name.clone())
        .collect();
    assert_eq!(
        names_after, names_before,
        "safe-mode session must not re-populate the skill registry from disk"
    );
}

// --- change_working_directory (#6032 / SEC-2) ---

/// FR-009: `/cd` with no argument reports the current cwd without mutating any state.
#[tokio::test]
async fn change_working_directory_empty_arg_reports_current_cwd() {
    use zeph_commands::traits::agent::AgentAccess;

    let harness = QuickTestAgent::minimal("ok");
    let mut agent = harness.agent;
    let original_cwd = std::env::current_dir().unwrap();

    let result = agent.change_working_directory("").await.unwrap();

    assert!(result.contains(&original_cwd.display().to_string()));
    assert_eq!(std::env::current_dir().unwrap(), original_cwd);
}

/// SEC-2: a `/cd` target outside `services.tool_state.allowed_paths` must be rejected and
/// must not mutate the process cwd — mirrors spec 063 FR-001/"Never": `/cd` must not become a
/// bypass for the per-path file sandbox.
#[tokio::test]
async fn change_working_directory_rejects_path_outside_allowed_paths() {
    use zeph_commands::traits::agent::AgentAccess;

    let harness = QuickTestAgent::minimal("ok");
    let mut agent = harness.agent;
    let original_cwd = std::env::current_dir().unwrap();

    let allowed_root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    agent.services.tool_state.allowed_paths = vec![allowed_root.path().to_path_buf()];

    let result = agent
        .change_working_directory(outside.path().to_str().unwrap())
        .await;

    assert!(result.is_err(), "cd outside allowed_paths must be rejected");
    assert_eq!(
        std::env::current_dir().unwrap(),
        original_cwd,
        "process cwd must be unchanged after a rejected /cd"
    );
}

/// A `/cd` target inside `allowed_paths` succeeds and drives the full post-change pipeline
/// (`check_cwd_changed`): `env_context.working_dir` and `runtime.lifecycle.last_known_cwd`
/// both reflect the new directory.
#[tokio::test]
async fn change_working_directory_allows_path_inside_allowed_paths() {
    use zeph_commands::traits::agent::AgentAccess;

    let harness = QuickTestAgent::minimal("ok");
    let mut agent = harness.agent;
    let original_cwd = std::env::current_dir().unwrap();

    let dir = tempfile::tempdir().unwrap();
    let canonical_dir = dir.path().canonicalize().unwrap();
    agent.services.tool_state.allowed_paths = vec![canonical_dir.clone()];

    let result = agent
        .change_working_directory(dir.path().to_str().unwrap())
        .await
        .unwrap();

    assert!(result.contains(&canonical_dir.display().to_string()));
    assert_eq!(agent.runtime.lifecycle.last_known_cwd, canonical_dir);
    assert_eq!(
        agent.services.session.env_context.working_dir,
        canonical_dir.display().to_string()
    );

    let _ = std::env::set_current_dir(&original_cwd);
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
