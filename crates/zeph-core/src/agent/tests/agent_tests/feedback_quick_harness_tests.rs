// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[allow(unused_imports)]
use super::*;

// --- QuickTestAgent ---

#[test]
fn agent_test_harness_minimal_constructs_agent() {
    let harness = QuickTestAgent::minimal("hello from mock");
    assert!(!harness.agent.msg.messages.is_empty());
    assert_eq!(harness.agent.msg.messages[0].role, Role::System);
}

#[test]
fn agent_test_harness_with_responses_constructs_agent() {
    let harness = QuickTestAgent::with_responses(vec!["first".into(), "second".into()]);
    assert!(!harness.agent.msg.messages.is_empty());
}

#[test]
fn agent_test_harness_sent_messages_initially_empty() {
    let harness = QuickTestAgent::minimal("response");
    assert!(harness.sent_messages().is_empty());
}
