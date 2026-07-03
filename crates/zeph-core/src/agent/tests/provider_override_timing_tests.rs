// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Regression test for #5548: ACP model-switch must apply within the same turn.
//!
//! `process_user_message` now calls `self.apply_provider_override()` as its first statement
//! (`crates/zeph-core/src/agent/mod.rs`), so a `provider_override` written after the loop-top
//! `apply_provider_override()` call but before the message reaches dispatch — e.g. an ACP
//! `session/set_config_option` write landing while the loop iteration is parked in
//! `next_event()` — is picked up before that same turn's LLM call, not one turn later.

use std::sync::Arc;

use zeph_llm::any::AnyProvider;

use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};

fn base_agent() -> crate::agent::Agent<MockChannel> {
    let provider = mock_provider(vec!["initial response".into()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    crate::agent::Agent::new(provider, channel, registry, None, 5, executor)
}

#[tokio::test]
async fn process_user_message_applies_override_written_just_before_it_is_called() {
    let slot: Arc<parking_lot::RwLock<Option<AnyProvider>>> =
        Arc::new(parking_lot::RwLock::new(None));
    let mut agent = base_agent().with_provider_override(Arc::clone(&slot));

    // Simulate a provider_override write that lands after the loop-top `apply_provider_override()`
    // call but before this turn's `process_user_message` dispatch (the exact race in #5548).
    *slot.write() = Some(mock_provider(vec!["override response".into()]));

    agent
        .process_user_message("hi".to_string(), vec![])
        .await
        .unwrap();

    let sent = agent.channel.sent_messages();
    assert!(
        sent.iter().any(|m| m.contains("override response")),
        "process_user_message must apply a provider_override written just before it runs, \
         within that same call — not one turn later; got: {sent:?}"
    );
    assert!(
        sent.iter().all(|m| !m.contains("initial response")),
        "the stale pre-override provider must not be used once an override is pending; got: {sent:?}"
    );
}

#[tokio::test]
async fn process_user_message_is_noop_override_when_slot_empty() {
    let slot: Arc<parking_lot::RwLock<Option<AnyProvider>>> =
        Arc::new(parking_lot::RwLock::new(None));
    let mut agent = base_agent().with_provider_override(Arc::clone(&slot));

    agent
        .process_user_message("hi".to_string(), vec![])
        .await
        .unwrap();

    let sent = agent.channel.sent_messages();
    assert!(
        sent.iter().any(|m| m.contains("initial response")),
        "with no pending override, the original provider must still be used; got: {sent:?}"
    );
}
