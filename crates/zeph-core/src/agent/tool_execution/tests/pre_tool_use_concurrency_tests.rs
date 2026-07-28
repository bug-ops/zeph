// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tests for #6259: `build_tier_call_futures` must fire `PreToolUse` hooks for every tool
//! call in a tier concurrently (Phase 1), not serially, before running the sequential
//! per-index gate-check loop (Phase 2). Mirrors `apply_tier_results_tests.rs`'s coverage of
//! the already-fixed `PostToolUse`/`RuntimeLayer::after_tool` twin (#6128).

#[cfg(unix)]
use std::path::Path;

#[cfg(unix)]
use tempfile::tempdir;
use zeph_config::{HookAction, HookDef, HookMatcher};
use zeph_llm::provider::{Message, MessagePart, Role, ToolUseRequest};

use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};

fn make_tool_use_request(id: &str, name: &str) -> ToolUseRequest {
    ToolUseRequest {
        id: id.into(),
        name: name.into(),
        input: serde_json::json!({}),
    }
}

/// Bound (in 50ms polling ticks) on each phase of the rendezvous barrier below: 60 ticks is
/// at least ~3s of wall time — real duration scales up under load, since each tick forks a
/// subshell to evaluate the glob.
#[cfg(unix)]
const BARRIER_PHASE_TICKS: u32 = 60;

/// A `PreToolUse` hook that proves n-way concurrency via a two-phase rendezvous barrier
/// (#6679) instead of racing a fixed sleep against subprocess-start spread. Each invocation:
/// 1. creates a uniquely-named marker file under `marker_dir`;
/// 2. spins (bounded by `BARRIER_PHASE_TICKS`) until it observes `n` markers — the arrival
///    barrier, satisfiable only once every hook invocation has started;
/// 3. on success, creates a uniquely-named witness file under `witness_dir`, then spins
///    (bounded) until it observes `n` witnesses — the departure barrier, which keeps every
///    marker alive until every invocation has individually confirmed the arrival barrier, so
///    no marker can be removed out from under a still-spinning sibling;
/// 4. removes its own marker and exits.
///
/// Serial dispatch can never satisfy the arrival barrier (hook 2..n are not scheduled until
/// hook 1 returns), so a regressed hook exhausts its bound without creating a witness, and the
/// test's `witness_count == n` assertion fails deterministically. Directory paths are quoted in
/// the shell template (#6679 review M2). `mktemp "<dir>"/marker.XXXXXX` is portable to both GNU
/// and BSD (macOS) `mktemp`; hooks run with `env_clear()` (hooks.rs), so the directory is passed
/// explicitly rather than relying on `$TMPDIR`.
#[cfg(unix)]
fn rendezvous_hook(marker_dir: &Path, witness_dir: &Path, n: usize) -> HookDef {
    let marker_dir = marker_dir.display();
    let witness_dir = witness_dir.display();
    let ticks = BARRIER_PHASE_TICKS;
    HookDef {
        action: HookAction::Command {
            command: format!(
                "f=$(mktemp \"{marker_dir}\"/marker.XXXXXX) && \
                 ok=0 && i=0 && \
                 while [ \"$i\" -lt {ticks} ]; do \
                   set -- \"{marker_dir}\"/marker.*; count=$#; \
                   if [ \"$count\" -ge {n} ]; then ok=1; break; fi; \
                   i=$((i + 1)); sleep 0.05; \
                 done && \
                 if [ \"$ok\" -eq 1 ]; then \
                   mktemp \"{witness_dir}\"/witness.XXXXXX >/dev/null && \
                   j=0 && \
                   while [ \"$j\" -lt {ticks} ]; do \
                     set -- \"{witness_dir}\"/witness.*; wcount=$#; \
                     if [ \"$wcount\" -ge {n} ]; then break; fi; \
                     j=$((j + 1)); sleep 0.05; \
                   done; \
                 fi; \
                 rm -f \"$f\""
            ),
        },
        timeout_secs: 25,
        fail_closed: false,
        r#if: None,
    }
}

/// N tool calls land in a single tier, each matching a `PreToolUse` hook that runs the
/// two-phase rendezvous barrier documented on [`rendezvous_hook`]. Concurrency is proven
/// structurally: every hook invocation must individually observe all `n` markers present
/// simultaneously to create its witness file, which is only reachable under concurrent
/// dispatch (Phase 1, bounded by the tier semaphore) — serial hook dispatch (the pre-#6259
/// behavior) can never satisfy it, since hook 2..n are not even scheduled until hook 1 returns.
/// This removes the fixed-sleep-vs-subprocess-spawn-spread race the original marker-file-poller
/// rewrite still carried (#6679 review, gap S1).
#[cfg(unix)]
#[tokio::test]
async fn pre_tool_use_hooks_fire_concurrently_across_tier_indices() {
    let n = 4;
    let marker_dir = tempdir().unwrap();
    let witness_dir = tempdir().unwrap();

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::new((0..n).map(|_| Ok(None)).collect());
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
    agent.runtime.config.timeouts.max_parallel_tools = n;
    agent.services.session.hooks_config.pre_tool_use = vec![HookMatcher {
        matcher: "noop".to_owned(),
        hooks: vec![rendezvous_hook(marker_dir.path(), witness_dir.path(), n)],
    }];
    agent
        .msg
        .messages
        .push(Message::from_legacy(Role::System, "system"));

    let tool_calls: Vec<ToolUseRequest> = (0..n)
        .map(|i| make_tool_use_request(&format!("id-{i}"), "noop"))
        .collect();

    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();

    let witness_count = std::fs::read_dir(witness_dir.path()).map_or(0, std::iter::Iterator::count);
    assert_eq!(
        witness_count, n,
        "every PreToolUse hook invocation must individually observe all {n} markers present \
         simultaneously — serial dispatch can never satisfy this rendezvous barrier"
    );

    // Every call must still have proceeded to execution (hook is fail_open and succeeds).
    let tool_result_count = agent
        .msg
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter(|p| matches!(p, MessagePart::ToolResult { .. }))
        .count();
    assert_eq!(
        tool_result_count, n,
        "every tool call must get a persisted ToolResult after its PreToolUse hook fires"
    );
}

/// Order invariant: a `fail_closed` `PreToolUse` hook block on one tier index must not affect
/// sibling indices in the same tier. Regression guard for the Phase 1 / Phase 2 split — Phase
/// 1 collects all blocked indices into a `HashMap<usize, String>` up front, and Phase 2 must
/// only consult the entry for its own idx.
#[tokio::test]
async fn pre_tool_use_hook_block_on_one_index_does_not_affect_siblings() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    // Only one call ("read") will actually reach the executor; "shell" is blocked before
    // dispatch by its fail_closed PreToolUse hook.
    let executor = MockToolExecutor::new(vec![Ok(None)]);
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
    agent.runtime.config.timeouts.max_parallel_tools = 2;
    agent.services.session.hooks_config.pre_tool_use = vec![HookMatcher {
        matcher: "shell".to_owned(),
        hooks: vec![HookDef {
            action: HookAction::Command {
                command: "exit 1".to_owned(),
            },
            timeout_secs: 5,
            fail_closed: true,
            r#if: None,
        }],
    }];
    agent
        .msg
        .messages
        .push(Message::from_legacy(Role::System, "system"));

    let tool_calls = vec![
        make_tool_use_request("id-shell", "shell"),
        make_tool_use_request("id-read", "read"),
    ];

    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();

    assert_eq!(
        agent.tool_orchestrator.hook_block_count, 1,
        "exactly one call (shell) must be blocked by its own fail_closed hook"
    );

    let tool_results: Vec<(&str, &str)> = agent
        .msg
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| {
            if let MessagePart::ToolResult {
                tool_use_id,
                content,
                ..
            } = p
            {
                Some((tool_use_id.as_str(), content.as_str()))
            } else {
                None
            }
        })
        .collect();

    let shell_result = tool_results
        .iter()
        .find(|(id, _)| *id == "id-shell")
        .expect("shell ToolResult must be present");
    assert!(
        shell_result.1.contains("[blocked]"),
        "shell call must be blocked by its own hook: {shell_result:?}"
    );

    let read_result = tool_results
        .iter()
        .find(|(id, _)| *id == "id-read")
        .expect("read ToolResult must be present");
    assert!(
        !read_result.1.contains("[blocked]"),
        "read call has no matching hook and must NOT be blocked by shell's hook: {read_result:?}"
    );
}
