// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tests for the MAGE trajectory-risk soft-escalation gate (spec 004-16 FR-006, #5956).
//!
//! `TrajectoryRiskAccumulator::should_escalate()`/`record_escalation()` existed but were never
//! queried by the agent loop before this fix. These tests exercise the wiring added to
//! `tier_loop.rs::check_mage_escalation` / `handle_native_tool_calls`, distinct from the
//! existing `is_blocked()` hard-block coverage in `zeph-memory::shadow::tests`.
//!
//! Critic finding F1 caught an earlier version of this wiring that synthesized
//! `ToolError::ConfirmationRequired` and dispatched approved calls through
//! `execute_tool_call_confirmed_erased`, which explicitly skips `check_trust` — letting a
//! policy-`Deny` tool execute under MAGE escalation. The fix gates the batch behind a single
//! up-front confirmation, then falls through to the normal `run_tier_execution_loop` so
//! `check_trust`/`PermissionPolicy` still apply per call. The `escalation_band_policy_*` tests
//! below are the regression coverage for that finding.

use std::collections::HashMap;

use zeph_config::tools::{AutonomyLevel, PermissionAction, PermissionRule};
use zeph_config::{
    TrajectoryRiskAccumulatorConfig, TrajectorySeverityMultipliers, TrajectorySignalWeights,
};
use zeph_llm::provider::{MessagePart, ToolUseRequest};
use zeph_memory::shadow::{AuditSignalType, Severity, TrajectoryRiskAccumulator};
use zeph_tools::{PermissionPolicy, TrustGateExecutor};

use crate::agent::Agent;
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

fn escalation_band_config() -> TrajectoryRiskAccumulatorConfig {
    TrajectoryRiskAccumulatorConfig {
        enabled: true,
        risk_threshold: 0.75,
        escalation_threshold: 0.50,
        risk_halflife_turns: 10,
        signal_history_cap: 200,
        tui_show_risk_gauge: true,
        reset_on_compaction: false,
        signal_weights: TrajectorySignalWeights::default(),
        severity_multipliers: TrajectorySeverityMultipliers::default(),
    }
}

/// Push the accumulator's risk into the escalation band `[0.50, 0.75)` without also
/// tripping the hard block.
fn accumulator_in_escalation_band() -> TrajectoryRiskAccumulator {
    let mut acc = TrajectoryRiskAccumulator::new(escalation_band_config());
    for _ in 0..2 {
        acc.advance_turn();
        acc.ingest(AuditSignalType::PolicyViolation, Severity::Medium);
    }
    assert!(
        acc.should_escalate(),
        "precondition: risk={} must land in escalation band",
        acc.current_risk()
    );
    assert!(!acc.is_blocked(), "precondition: must not also hard-block");
    acc
}

fn make_agent_with_confirmations(confirmations: Vec<bool>) -> Agent<MockChannel> {
    Agent::new(
        mock_provider(vec![]),
        MockChannel::new(vec![]).with_confirmations(confirmations),
        create_test_registry(),
        None,
        5,
        MockToolExecutor::with_output("shell", "ok"),
    )
}

/// Build an agent whose tool executor is wrapped in a real `TrustGateExecutor`, so
/// `PermissionPolicy` Ask/Deny rules are actually enforced — needed to prove MAGE escalation
/// does not bypass them (critic finding F1).
fn make_agent_with_policy(
    policy: PermissionPolicy,
    confirmations: Vec<bool>,
) -> Agent<MockChannel> {
    let executor = TrustGateExecutor::new(MockToolExecutor::with_output("bash", "ok"), policy);
    Agent::new(
        mock_provider(vec![]),
        MockChannel::new(vec![]).with_confirmations(confirmations),
        create_test_registry(),
        None,
        5,
        executor,
    )
}

fn deny_policy_for_bash() -> PermissionPolicy {
    let mut rules = HashMap::new();
    rules.insert(
        "bash".to_owned(),
        vec![PermissionRule {
            pattern: "*".to_owned(),
            action: PermissionAction::Deny,
        }],
    );
    PermissionPolicy::new(rules).with_autonomy(AutonomyLevel::Supervised)
}

fn ask_policy_for_bash() -> PermissionPolicy {
    let mut rules = HashMap::new();
    rules.insert(
        "bash".to_owned(),
        vec![PermissionRule {
            pattern: "*".to_owned(),
            action: PermissionAction::Ask,
        }],
    );
    PermissionPolicy::new(rules).with_autonomy(AutonomyLevel::Supervised)
}

fn tool_result_contains(agent: &Agent<MockChannel>, needle: &str) -> bool {
    agent.msg.messages.iter().any(|m| {
        m.parts.iter().any(
            |p| matches!(p, MessagePart::ToolResult { content, .. } if content.contains(needle)),
        )
    })
}

#[tokio::test]
async fn escalation_band_requires_confirmation_and_executes_on_approval() {
    let mut agent = make_agent_with_confirmations(vec![true]);
    agent.services.security.mage_accumulator = accumulator_in_escalation_band();

    let tool_calls = vec![make_tool_use_request("id-1", "shell")];
    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();

    // Confirmation queue must have been consumed — proves Channel::confirm was invoked.
    assert!(
        agent.channel.confirmations.lock().unwrap().is_empty(),
        "escalation band must trigger exactly one confirmation prompt"
    );
    assert!(
        !tool_result_contains(&agent, "[Cancelled]"),
        "approved escalation must execute the tool, not cancel it"
    );
}

#[tokio::test]
async fn escalation_band_deny_cancels_tool_without_executing() {
    let mut agent = make_agent_with_confirmations(vec![false]);
    agent.services.security.mage_accumulator = accumulator_in_escalation_band();

    let tool_calls = vec![make_tool_use_request("id-2", "shell")];
    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();

    assert!(
        tool_result_contains(&agent, "[Cancelled]"),
        "denied escalation confirmation must cancel the whole batch"
    );
}

#[tokio::test]
async fn below_escalation_threshold_does_not_prompt_for_confirmation() {
    // Regression control: the default (noop) accumulator must never trigger the escalation
    // path — the queued "deny" confirmation must go unused.
    let mut agent = make_agent_with_confirmations(vec![false]);

    let tool_calls = vec![make_tool_use_request("id-3", "shell")];
    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();

    assert_eq!(
        agent.channel.confirmations.lock().unwrap().len(),
        1,
        "no confirmation must be requested when trajectory risk is below the escalation threshold"
    );
    assert!(
        !tool_result_contains(&agent, "[Cancelled]"),
        "tool must execute normally when shadow memory is disabled"
    );
}

#[tokio::test]
async fn hard_block_takes_precedence_over_escalation() {
    // When risk >= risk_threshold, the hard block (is_blocked/TrajectoryRiskExceeded) must
    // fire instead of the soft-escalation confirmation path — the two tiers are mutually
    // exclusive and the hard block always wins. Tester feedback: exercise this right at the
    // boundary (risk == risk_threshold exactly), not just deep in the clamped range — a config
    // with risk_threshold == a single signal's raw weight lands exactly on the boundary with no
    // clamping involved, unlike 5x high-severity signals which only prove the clamped-far-above
    // case.
    let mut agent = make_agent_with_confirmations(vec![true]);
    let mut tight_config = escalation_band_config();
    tight_config.escalation_threshold = 0.15;
    tight_config.risk_threshold = 0.30;
    let mut acc = TrajectoryRiskAccumulator::new(tight_config);
    acc.advance_turn();
    // PolicyViolation Medium = 0.30 * 1.0 = 0.30, exactly equal to risk_threshold above.
    acc.ingest(AuditSignalType::PolicyViolation, Severity::Medium);
    assert!(
        (acc.current_risk() - 0.30).abs() < 1e-9,
        "precondition: risk={} must land exactly at risk_threshold",
        acc.current_risk()
    );
    assert!(
        acc.is_blocked(),
        "precondition: risk={}",
        acc.current_risk()
    );
    agent.services.security.mage_accumulator = acc;

    let tool_calls = vec![make_tool_use_request("id-4", "shell")];
    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();

    assert_eq!(
        agent.channel.confirmations.lock().unwrap().len(),
        1,
        "hard block must not consume the confirmation queue"
    );
    assert!(
        tool_result_contains(&agent, "trajectory risk"),
        "hard block must produce a TrajectoryRiskExceeded tool result"
    );
}

#[tokio::test]
async fn escalation_band_policy_deny_tool_never_executes_on_approval() {
    // F1 regression: a policy-Deny tool must still be hard-blocked by check_trust inside the
    // normal tier loop, even after the user approves the MAGE escalation prompt.
    let mut agent = make_agent_with_policy(deny_policy_for_bash(), vec![true]);
    agent.services.security.mage_accumulator = accumulator_in_escalation_band();

    let tool_calls = vec![make_tool_use_request("id-5", "bash")];
    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();

    assert!(
        tool_result_contains(&agent, "blocked by policy"),
        "a policy-Deny tool must be blocked even after MAGE escalation is approved"
    );
    assert!(
        !tool_result_contains(&agent, "\nok\n"),
        "the denied tool must never actually execute"
    );
}

#[tokio::test]
async fn escalation_band_policy_deny_tool_never_executes_under_auto_approve() {
    // F1 regression, auto-approve variant: with no confirmations queued, MockChannel::confirm
    // auto-approves every prompt (mirrors -y/--bare/non-TTY CLI/JSON-CLI auto-approve modes).
    // A policy-Deny tool must still never execute — this is the "unattended execution" scenario
    // the critic flagged as the worst-case consequence of the original bypass.
    let mut agent = make_agent_with_policy(deny_policy_for_bash(), vec![]);
    agent.services.security.mage_accumulator = accumulator_in_escalation_band();

    let tool_calls = vec![make_tool_use_request("id-6", "bash")];
    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();

    assert!(
        tool_result_contains(&agent, "blocked by policy"),
        "a policy-Deny tool must be blocked under auto-approve, not silently executed"
    );
    assert!(
        !tool_result_contains(&agent, "\nok\n"),
        "the denied tool must never actually execute under auto-approve"
    );
}

#[tokio::test]
async fn escalation_band_policy_ask_shows_real_command_in_prompt() {
    // F1/F2 regression: the per-call policy-Ask confirmation must show the real command, not
    // the generic MAGE batch-level prompt string.
    let mut agent = make_agent_with_policy(ask_policy_for_bash(), vec![true, true]);
    agent.services.security.mage_accumulator = accumulator_in_escalation_band();

    let tool_calls = vec![ToolUseRequest {
        id: "id-7".into(),
        name: "bash".into(),
        input: serde_json::json!({ "command": "rm -rf /some/real/path" }),
    }];
    agent
        .handle_native_tool_calls(None, &tool_calls)
        .await
        .unwrap();

    let prompts = agent.channel.confirmed_prompts();
    assert_eq!(
        prompts.len(),
        2,
        "expected one MAGE batch-level prompt + one per-call policy-Ask prompt: {prompts:?}"
    );
    assert!(
        !prompts[0].contains("rm -rf"),
        "the MAGE batch-level prompt must be generic, not leak the real command: {:?}",
        prompts[0]
    );
    assert!(
        prompts[1].contains("rm -rf /some/real/path"),
        "the per-call policy-Ask prompt must show the real command: {:?}",
        prompts[1]
    );
    assert!(
        tool_result_contains(&agent, "name=\"bash\"") && tool_result_contains(&agent, "\nok\n"),
        "approving both prompts must let the tool execute"
    );
}
