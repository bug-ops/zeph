// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Regression tests for #5551: `--bare` mode must not fire shutdown-path LLM calls.
//!
//! Covers all four shutdown-path subsystems gated on `self.runtime.config.bare`:
//! `maybe_autodream`, `maybe_extract_skills_from_trace`, `maybe_store_shutdown_summary`, and
//! `maybe_store_session_digest`. Each checks `bare` before its own subsystem-enabled checks.
//! These tests prove the `bare` gate short-circuits even when the subsystem's own config flag
//! is `true` — the exact reproduction scenario from the issue — and that a non-bare agent still
//! reaches the gated work. The `shutdown_summary`/`session_digest` gap (two of the four
//! subsystems originally left ungated) was found in adversarial review — see
//! `.local/handoff/2026-07-03T10-55-16-critic.md`.

use std::sync::Arc;

use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{Message, MessageMetadata, Role};
use zeph_memory::semantic::SemanticMemory;

use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};
use crate::config::LearningConfig;

fn base_agent() -> crate::agent::Agent<MockChannel> {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    crate::agent::Agent::new(provider, channel, registry, None, 5, executor)
}

#[tokio::test]
async fn maybe_autodream_skips_when_bare_even_if_enabled() {
    let mut agent = base_agent().with_bare_mode(true);
    agent.services.memory.subsystems.autodream_config.enabled = true;

    agent.maybe_autodream().await;

    assert_eq!(
        agent
            .services
            .memory
            .subsystems
            .autodream
            .sessions_since_consolidation,
        0,
        "bare mode must return before record_session() runs, even with autodream.enabled = true"
    );
}

#[tokio::test]
async fn maybe_autodream_runs_gate_check_when_not_bare() {
    let mut agent = base_agent().with_bare_mode(false);
    agent.services.memory.subsystems.autodream_config.enabled = true;

    agent.maybe_autodream().await;

    assert_eq!(
        agent
            .services
            .memory
            .subsystems
            .autodream
            .sessions_since_consolidation,
        1,
        "non-bare agent must reach record_session() when autodream is enabled"
    );
}

#[tokio::test]
async fn maybe_extract_skills_from_trace_skips_when_bare_even_if_enabled() {
    let managed = tempfile::tempdir().unwrap();
    let mut agent = base_agent()
        .with_bare_mode(true)
        .with_managed_skills_dir(managed.path().to_path_buf());
    agent.services.learning_engine.config = Some(LearningConfig {
        trace_extraction_enabled: true,
        ..Default::default()
    });
    agent.services.memory.persistence.conversation_id = Some(zeph_memory::ConversationId(1));
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "hello".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    agent.maybe_extract_skills_from_trace().await;

    assert!(
        agent
            .services
            .learning_engine
            .trace_extraction_handle
            .is_none(),
        "bare mode must return before spawning the trace-extraction task, even with \
         trace_extraction_enabled = true"
    );
}

#[tokio::test]
async fn maybe_extract_skills_from_trace_spawns_when_not_bare() {
    let managed = tempfile::tempdir().unwrap();
    let mut agent = base_agent()
        .with_bare_mode(false)
        .with_managed_skills_dir(managed.path().to_path_buf());
    agent.services.learning_engine.config = Some(LearningConfig {
        trace_extraction_enabled: true,
        ..Default::default()
    });
    agent.services.memory.persistence.conversation_id = Some(zeph_memory::ConversationId(1));
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "hello".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    agent.maybe_extract_skills_from_trace().await;

    assert!(
        agent
            .services
            .learning_engine
            .trace_extraction_handle
            .is_some(),
        "non-bare agent must reach the spawn point when trace_extraction_enabled is true"
    );
}

/// Build an in-memory `SemanticMemory` + conversation id for shutdown-summary/digest tests,
/// which both require `persistence.memory`/`persistence.conversation_id` to be `Some` before
/// their own config checks run.
async fn memory_with_conversation() -> (Arc<SemanticMemory>, zeph_memory::ConversationId) {
    let memory = SemanticMemory::new(
        ":memory:",
        "http://127.0.0.1:1",
        None,
        AnyProvider::Mock(MockProvider::default()),
        "test-model",
    )
    .await
    .unwrap();
    let cid = memory.sqlite().create_conversation().await.unwrap();
    (Arc::new(memory), cid)
}

#[tokio::test]
async fn maybe_store_shutdown_summary_skips_when_bare_even_if_enabled() {
    let (mock, recorded) = MockProvider::default().with_recording();
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (memory, cid) = memory_with_conversation().await;

    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor)
        .with_bare_mode(true)
        .with_memory(memory, cid, 100, 5, 1000)
        .with_shutdown_summary_config(true, 4, 20, 10);

    // >= min_messages (4) user turns so the non-bare path would otherwise proceed.
    for i in 0..5 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("user message {i}"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    agent.maybe_store_shutdown_summary().await;

    assert!(
        recorded.lock().unwrap().is_empty(),
        "bare mode must return before any LLM call, even with shutdown_summary enabled and \
         the min_messages threshold met"
    );
}

#[tokio::test]
async fn maybe_store_shutdown_summary_runs_when_not_bare() {
    let (mock, recorded) = MockProvider::default().with_recording();
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (memory, cid) = memory_with_conversation().await;

    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor)
        .with_bare_mode(false)
        .with_memory(memory, cid, 100, 5, 1000)
        .with_shutdown_summary_config(true, 4, 20, 10);

    for i in 0..5 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("user message {i}"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    agent.maybe_store_shutdown_summary().await;

    assert!(
        !recorded.lock().unwrap().is_empty(),
        "non-bare agent must reach the LLM call when shutdown_summary is enabled and the \
         min_messages threshold is met"
    );
}

#[tokio::test]
async fn maybe_store_session_digest_skips_when_bare_even_if_enabled() {
    let (mock, recorded) = MockProvider::default().with_recording();
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (memory, cid) = memory_with_conversation().await;

    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor)
        .with_bare_mode(true)
        .with_memory(memory, cid, 100, 5, 1000);
    agent.services.memory.compaction.digest_config.enabled = true;
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "hello".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    agent.maybe_store_session_digest().await;

    assert!(
        recorded.lock().unwrap().is_empty(),
        "bare mode must return before any LLM call, even with digest_config.enabled = true"
    );
}

#[tokio::test]
async fn maybe_store_session_digest_runs_when_not_bare() {
    let (mock, recorded) = MockProvider::default().with_recording();
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (memory, cid) = memory_with_conversation().await;

    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor)
        .with_bare_mode(false)
        .with_memory(memory, cid, 100, 5, 1000);
    agent.services.memory.compaction.digest_config.enabled = true;
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "hello".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    agent.maybe_store_session_digest().await;

    assert!(
        !recorded.lock().unwrap().is_empty(),
        "non-bare agent must reach the LLM call when digest_config.enabled = true"
    );
}
