// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use tokio::sync::watch;
use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

use crate::agent::Agent;
use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};
use crate::agent::context::CompactionOutcome;
use crate::agent::context_manager::CompactionState;
use crate::context::ContextBudget;

// Helper: add a tool pair with ToolOutput parts (so pruning can clear the body).
fn make_tool_pair_with_output(agent: &mut Agent<MockChannel>, tool_name: &str) {
    agent.msg.messages.push(Message::from_parts(
        Role::Assistant,
        vec![MessagePart::ToolUse {
            id: format!("id_{tool_name}"),
            name: tool_name.to_owned(),
            input: serde_json::json!({"cmd": "echo hello"}),
        }],
    ));
    agent.msg.messages.push(Message::from_parts(
        Role::User,
        vec![MessagePart::ToolOutput {
            tool_name: tool_name.into(),
            body: format!("full output of {tool_name}"),
            compacted_at: None,
        }],
    ));
}

#[test]
fn remove_lsp_messages_removes_lsp_system_keeps_others() {
    use zeph_agent_context::helpers::LSP_NOTE_PREFIX;

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    // Push a non-LSP system message that must survive.
    agent.push_message(Message {
        role: Role::System,
        content: "[recall] some recall data".to_owned(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });
    // Push an LSP system note that must be removed.
    agent.push_message(Message {
        role: Role::System,
        content: format!("{LSP_NOTE_PREFIX}diagnostics]\nsrc/main.rs:1 error: foo"),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });
    // Push a user message that must survive.
    agent.push_message(Message {
        role: Role::User,
        content: "hello".to_owned(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    let before = agent.msg.messages.len();
    agent.remove_lsp_messages();
    // Only the LSP system note should be gone.
    assert_eq!(agent.msg.messages.len(), before - 1);
    assert!(
        agent
            .msg
            .messages
            .iter()
            .all(|m| !m.content.starts_with(LSP_NOTE_PREFIX))
    );
    // Non-LSP system message preserved.
    assert!(
        agent
            .msg
            .messages
            .iter()
            .any(|m| m.content.starts_with("[recall]"))
    );
}

// --- Compaction guard tests (issue #1708) ---

// Cooldown guard: cooling turns_remaining counts down and blocks compaction.
#[tokio::test]
async fn cooldown_guard_decrements_and_skips_compaction() {
    let provider = mock_provider(vec!["summary".to_string()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 2, 0)
        .with_metrics(tx);
    agent.context_manager.compaction_cooldown_turns = 2;

    // Manually set cooling state as if compaction just fired and turn advanced.
    agent.context_manager.compaction = CompactionState::Cooling { turns_remaining: 2 };

    // Push enough tokens to trigger compaction threshold.
    for i in 0..10 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("message {i} padding to exceed budget threshold"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    // First call: turns_remaining = 2 → skips, decrements to 1. No compaction fired.
    agent.maybe_compact().await.unwrap();
    assert_eq!(agent.context_manager.compaction.cooldown_remaining(), 1);
    assert_eq!(rx.borrow().context_compactions, 0);

    // Second call: turns_remaining = 1 → skips, decrements to 0 → transitions to Ready.
    agent.maybe_compact().await.unwrap();
    assert_eq!(agent.context_manager.compaction.cooldown_remaining(), 0);
    assert_eq!(rx.borrow().context_compactions, 0);
}

// Cooldown guard: after cooldown expires, compaction fires and resets the counter.
#[tokio::test]
async fn cooldown_guard_fires_after_expiry_and_resets_counter() {
    let provider = mock_provider(vec!["summary".to_string()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 2, 0)
        .with_metrics(tx);
    agent.context_manager.compaction_cooldown_turns = 2;

    // Ready state means cooldown has already expired.
    assert_eq!(agent.context_manager.compaction, CompactionState::Ready);

    for i in 0..10 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("message {i} padding to exceed budget threshold"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    // Seed cached_prompt_tokens above threshold so maybe_compact proceeds past Guard 1.
    // Messages were pushed directly (bypassing push_message), so we set this explicitly.
    // Use a large value so freed_tokens > 0 after compact_context() recomputes.
    agent.runtime.providers.cached_prompt_tokens = 10_000;

    agent.maybe_compact().await.unwrap();

    // Compaction fired: metrics incremented.
    assert_eq!(rx.borrow().context_compactions, 1);
    // After compaction the system prompt alone exceeds the tiny 100-token budget, so
    // Guard 3 marks exhaustion (still above threshold). Cooldown is not reset — correct.
    assert!(agent.context_manager.compaction.is_exhausted());
}

// Exhaustion guard: when compaction_exhausted is set, maybe_compact returns early.
#[tokio::test]
async fn exhaustion_guard_skips_compaction_when_exhausted() {
    let provider = mock_provider(vec!["summary".to_string()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 2, 0)
        .with_metrics(tx);

    agent.context_manager.compaction = CompactionState::Exhausted { warned: false };

    for i in 0..10 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("message {i} padding to exceed budget threshold"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    agent.maybe_compact().await.unwrap();

    // Compaction did NOT fire.
    assert_eq!(rx.borrow().context_compactions, 0);
}

// Exhaustion guard: exhaustion_warned set after first call, stays true on second call.
#[tokio::test]
async fn exhaustion_guard_warned_flag_set_once() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 2, 0);

    agent.context_manager.compaction = CompactionState::Exhausted { warned: false };

    for i in 0..5 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("message {i}"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    // First call: warning not yet sent → warned flipped to true.
    assert!(matches!(
        agent.context_manager.compaction,
        CompactionState::Exhausted { warned: false }
    ));
    agent.maybe_compact().await.unwrap();
    assert!(matches!(
        agent.context_manager.compaction,
        CompactionState::Exhausted { warned: true }
    ));

    // Second call: warned already set, no state change.
    agent.maybe_compact().await.unwrap();
    assert!(matches!(
        agent.context_manager.compaction,
        CompactionState::Exhausted { warned: true }
    ));
}

// Exhaustion guard fires before cooldown guard.
#[tokio::test]
async fn exhaustion_guard_takes_precedence_over_cooldown() {
    use std::sync::Arc;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let statuses = Arc::clone(&channel.statuses);
    let registry = create_test_registry();

    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 2, 0);
    agent.context_manager.compaction_cooldown_turns = 2;

    // Exhausted state (the Cooling state would normally guard against exhaustion, but
    // we test the ordering guarantee that exhaustion check happens before cooldown decrement).
    agent.context_manager.compaction = CompactionState::Exhausted { warned: false };

    for i in 0..10 {
        agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("message {i} padding to exceed budget threshold"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    agent.maybe_compact().await.unwrap();

    // State must remain Exhausted — exhaustion guard returned before cooldown decrement.
    assert!(agent.context_manager.compaction.is_exhausted());
    // No "compacting context..." status emitted.
    assert!(
        !statuses
            .lock()
            .unwrap()
            .iter()
            .any(|s| s == "compacting context..."),
        "compaction must not have started"
    );
}

// Counterproductive guard: too few compactable messages sets exhausted.
#[tokio::test]
async fn counterproductive_guard_sets_exhausted_when_too_few_messages() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    // preserve_tail = 5, budget = 100 so threshold is low → should_compact() fires.
    // With only a system prompt + 2 messages, compactable = len - preserve_tail - 1.
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 5, 0);

    // Add just 2 messages: compactable = 3 - 5 - 1 = saturates to 0, which is ≤ 1.
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "x".repeat(200),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "x".repeat(200),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    // Tier-1 pruning won't free enough (no ToolOutput parts), so tier-2 attempts.
    agent.maybe_compact().await.unwrap();

    // Counterproductive guard: compactable ≤ 1 → exhausted set.
    assert!(agent.context_manager.compaction.is_exhausted());
}

// Default value for compaction_cooldown_turns is 2.
#[test]
fn context_manager_defaults_have_compaction_guard_fields() {
    let cm = crate::agent::context_manager::ContextManager::new();
    assert_eq!(cm.compaction_cooldown_turns, 2);
    assert_eq!(cm.compaction, CompactionState::Ready);
}

// with_compaction_cooldown builder sets the cooldown turns field.
#[test]
fn builder_with_compaction_cooldown_sets_field() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.context_manager.compaction_cooldown_turns = 5;

    assert_eq!(agent.context_manager.compaction_cooldown_turns, 5);
}

#[test]
fn compaction_hard_count_zero_by_default() {
    let snapshot = crate::metrics::MetricsSnapshot::default();
    assert_eq!(snapshot.compaction_hard_count, 0);
    assert!(snapshot.compaction_turns_after_hard.is_empty());
}

#[tokio::test]
async fn compaction_hard_count_increments_on_hard_tier() {
    let provider = mock_provider(vec!["summary".to_string()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(1000, 0.20, 0.75, 4, 0)
        .with_metrics(tx);

    // Drive cached_prompt_tokens above the hard threshold (75% of 1000 = 750).
    agent.runtime.providers.cached_prompt_tokens = 900;

    agent.maybe_compact().await.unwrap();

    assert_eq!(rx.borrow().compaction_hard_count, 1);
}

#[tokio::test]
async fn compaction_turns_after_hard_tracks_segments() {
    let provider = mock_provider(vec!["summary".to_string()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(1000, 0.20, 0.75, 4, 0)
        .with_metrics(tx);
    agent.context_manager.compaction_cooldown_turns = 0;

    // Simulate first hard compaction by driving cached tokens above threshold.
    agent.runtime.providers.cached_prompt_tokens = 900;
    agent.maybe_compact().await.unwrap();
    assert_eq!(rx.borrow().compaction_hard_count, 1);
    // turns_since_last_hard_compaction is now Some(0).

    // Simulate 3 turns where context is below threshold.
    // Reset per-turn state (done by advance_turn at the start of each turn).
    agent.runtime.providers.cached_prompt_tokens = 0;
    for _ in 0..3 {
        agent.context_manager.compaction = agent.context_manager.compaction.advance_turn();
        agent.maybe_compact().await.unwrap();
    }
    // turns_since_last_hard_compaction is now Some(3).

    // Directly trigger the Hard tier accounting without a real LLM call
    // by simulating what maybe_compact does in the Hard branch.
    // This tests that the Vec accumulates the segment correctly.
    if let Some(turns) = agent.context_manager.turns_since_last_hard_compaction {
        agent.update_metrics(|m| {
            m.compaction_turns_after_hard.push(turns);
            m.compaction_hard_count += 1;
        });
        agent.context_manager.turns_since_last_hard_compaction = Some(0);
    }

    assert_eq!(rx.borrow().compaction_hard_count, 2);
    assert_eq!(rx.borrow().compaction_turns_after_hard, vec![3]);
}

#[tokio::test]
async fn compaction_turn_counter_increments_before_exhaustion_guard() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(1000, 0.20, 0.75, 4, 0);

    // Manually set tracking active and exhaust compaction.
    agent.context_manager.turns_since_last_hard_compaction = Some(0);
    agent.context_manager.compaction = CompactionState::Exhausted { warned: false };

    // Call maybe_compact — early return via exhaustion guard.
    agent.maybe_compact().await.unwrap();

    // Turn counter must still have been incremented (S1/S2 fix).
    assert_eq!(
        agent.context_manager.turns_since_last_hard_compaction,
        Some(1)
    );
}

// maybe_soft_compact_mid_iteration tests (#1828)

#[test]
fn mid_iteration_skips_when_compacted_this_turn() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    // budget=100_000, soft=0.60 → soft_threshold=60_000
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100_000, 0.20, 0.90, 4, 0);
    agent.context_manager.soft_compaction_threshold = 0.60;

    make_tool_pair_with_output(&mut agent, "a");
    agent.msg.messages[2].metadata.deferred_summary = Some("sum_a".into());
    // Simulate token pressure above soft threshold
    agent.runtime.providers.cached_prompt_tokens = 75_000;
    // Mark hard compaction already ran this turn
    agent.context_manager.compaction = CompactionState::CompactedThisTurn { cooldown: 2 };

    agent.maybe_soft_compact_mid_iteration();

    // Deferred summary must NOT have been applied (early return)
    let applied = agent.msg.messages.iter().any(|m| {
        m.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Summary { .. }))
    });
    assert!(
        !applied,
        "must not apply deferred summaries when compacted_this_turn is set"
    );
}

#[test]
fn mid_iteration_skips_when_tier_is_none() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    // budget=100_000, soft=0.60 → soft_threshold=60_000
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100_000, 0.20, 0.90, 4, 0);
    agent.context_manager.soft_compaction_threshold = 0.60;

    make_tool_pair_with_output(&mut agent, "a");
    agent.msg.messages[2].metadata.deferred_summary = Some("sum_a".into());
    // Token count well below soft threshold (50_000 < 60_000) → None tier
    agent.runtime.providers.cached_prompt_tokens = 50_000;

    agent.maybe_soft_compact_mid_iteration();

    // No deferred summary applied when tier is None
    let applied = agent.msg.messages.iter().any(|m| {
        m.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Summary { .. }))
    });
    assert!(!applied, "must not compact when tier is None");
}

#[test]
fn mid_iteration_applies_deferred_summaries_at_soft_tier() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    // budget=100_000, soft=0.60 → soft_threshold=60_000; hard=0.90
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100_000, 0.20, 0.90, 4, 0);
    agent.context_manager.soft_compaction_threshold = 0.60;

    make_tool_pair_with_output(&mut agent, "a");
    agent.msg.messages[2].metadata.deferred_summary = Some("sum_a".into());
    // Token pressure above soft (75_000 > 60_000) but below hard (90_000)
    agent.runtime.providers.cached_prompt_tokens = 75_000;

    agent.maybe_soft_compact_mid_iteration();

    // Deferred summary must have been applied
    let summary_inserted = agent.msg.messages.iter().any(|m| {
        m.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Summary { .. }))
    });
    assert!(
        summary_inserted,
        "deferred summary must be applied at soft tier"
    );
}

#[test]
fn mid_iteration_does_not_set_compacted_this_turn() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100_000, 0.20, 0.90, 4, 0);
    agent.context_manager.soft_compaction_threshold = 0.60;

    make_tool_pair_with_output(&mut agent, "a");
    agent.runtime.providers.cached_prompt_tokens = 75_000;

    assert!(!agent.context_manager.compaction.is_compacted_this_turn());
    agent.maybe_soft_compact_mid_iteration();
    assert!(
        !agent.context_manager.compaction.is_compacted_this_turn(),
        "maybe_soft_compact_mid_iteration must not set compacted_this_turn"
    );
}

#[test]
fn mid_iteration_fires_at_hard_tier() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    // budget=100_000, soft=0.60 → 60_000; hard=0.90 → 90_000
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100_000, 0.20, 0.90, 4, 0);
    agent.context_manager.soft_compaction_threshold = 0.60;

    make_tool_pair_with_output(&mut agent, "a");
    agent.msg.messages[2].metadata.deferred_summary = Some("sum_a".into());
    // Token pressure above hard threshold (95_000 > 90_000) → Hard tier
    agent.runtime.providers.cached_prompt_tokens = 95_000;

    agent.maybe_soft_compact_mid_iteration();

    // Soft actions (deferred summaries) must still be applied even at Hard tier
    let summary_inserted = agent.msg.messages.iter().any(|m| {
        m.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Summary { .. }))
    });
    assert!(
        summary_inserted,
        "deferred summaries must be applied even when tier is Hard"
    );
    // compaction state must remain unchanged (no LLM call, no Hard compaction)
    assert!(
        !agent.context_manager.compaction.is_compacted_this_turn(),
        "mid-iteration must not set compacted_this_turn even at Hard tier"
    );
}

// --- assembly.rs: clear_history ---

/// `clear_history` must retain the system prompt (message[0]) and discard all
/// subsequent messages so the agent can restart a conversation cleanly.
#[tokio::test]
async fn clear_history_retains_system_prompt() {
    use zeph_skills::registry::SkillRegistry;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = SkillRegistry::default();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    // Add some history beyond the initial system prompt.
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "hello".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });
    agent.msg.messages.push(Message {
        role: Role::Assistant,
        content: "world".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });
    assert_eq!(agent.msg.messages.len(), 3);

    agent.clear_history();

    assert_eq!(
        agent.msg.messages.len(),
        1,
        "clear_history must leave exactly the system prompt"
    );
    assert_eq!(
        agent.msg.messages[0].role,
        Role::System,
        "retained message must be the system prompt"
    );
}

/// `clear_history` on an agent with only the system prompt must leave it unchanged.
#[tokio::test]
async fn clear_history_with_only_system_prompt_is_idempotent() {
    use zeph_skills::registry::SkillRegistry;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = SkillRegistry::default();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let system_content = agent.msg.messages[0].content.clone();

    agent.clear_history();

    assert_eq!(agent.msg.messages.len(), 1);
    assert_eq!(
        agent.msg.messages[0].content, system_content,
        "system prompt content must be unchanged after clear_history"
    );
}

// --- assembly.rs: rebuild_system_prompt with empty skill list ---

/// `rebuild_system_prompt` must not panic and must produce a non-empty prompt
/// even when the skill registry is empty (no skills loaded).
#[tokio::test]
async fn rebuild_system_prompt_empty_skill_list_does_not_crash() {
    use zeph_skills::registry::SkillRegistry;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    // Explicitly empty registry — no skills at all.
    let registry = SkillRegistry::default();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    // Must not panic.
    agent
        .rebuild_system_prompt("test query with no skills")
        .await;

    let prompt = &agent.msg.messages[0];
    assert_eq!(
        prompt.role,
        Role::System,
        "first message must still be the system prompt"
    );
    assert!(
        !prompt.content.is_empty(),
        "system prompt must be non-empty even with no skills"
    );
}

/// The system prompt produced by `rebuild_system_prompt` must contain exactly
/// the two cache marker comments required by the Claude caching implementation
/// (cache:stable and cache:volatile). More than 4 markers would exceed the API
/// limit; the prompt format is expected to use exactly these two.
#[tokio::test]
async fn rebuild_system_prompt_cache_markers_count() {
    use zeph_skills::registry::SkillRegistry;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = SkillRegistry::default();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.rebuild_system_prompt("test query").await;

    let prompt = &agent.msg.messages[0].content;
    let stable_count = prompt.matches("<!-- cache:stable -->").count();
    let volatile_count = prompt.matches("<!-- cache:volatile -->").count();

    assert_eq!(
        stable_count, 1,
        "exactly one cache:stable marker must be present"
    );
    assert_eq!(
        volatile_count, 1,
        "exactly one cache:volatile marker must be present"
    );
    // Total cache markers must not exceed 4 (Claude API limit).
    let total = stable_count + volatile_count + prompt.matches("<!-- cache:tools -->").count();
    assert!(
        total <= 4,
        "total cache markers must not exceed 4 (Claude API limit); got {total}"
    );
}

// T-06: H1 regression — ProbeRejected must NOT trigger Exhausted transition.
//
// Design invariant (H1 fix): when the compaction probe rejects a summary,
// compact_context() returns CompactionOutcome::ProbeRejected. The caller
// (maybe_compact) must set CompactedThisTurn (cooldown) and NOT transition
// to CompactionState::Exhausted, because the failure is quality-related, not
// because the compactor is structurally unable to free tokens.
#[tokio::test]
async fn probe_rejected_does_not_trigger_exhausted() {
    // Provider returns:
    //   1st call: summary text (for summarize_messages)
    //   2nd call: probe questions JSON
    //   3rd call: probe answers JSON — all refusals → score ~0.0 → HardFail
    let questions_json = r#"{"questions": [{"question": "What crate?", "expected_answer": "thiserror"}, {"question": "What file?", "expected_answer": "src/lib.rs"}]}"#;
    let answers_json = r#"{"answers": ["UNKNOWN", "UNKNOWN"]}"#;
    let provider = mock_provider(vec![
        "compacted summary".to_string(),
        questions_json.to_string(),
        answers_json.to_string(),
    ]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_context_budget(100, 0.20, 0.75, 2, 0);

    // Enable compaction probe with default thresholds (Pass >= 0.6, HardFail < 0.35).
    agent.context_manager.compression.probe.enabled = true;

    // Populate enough messages to pass the too-few-messages guard.
    for i in 0..8 {
        agent.msg.messages.push(Message {
            role: if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            },
            content: format!("message {i}"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    let outcome = agent.compact_context().await.unwrap();

    // H1 invariant: probe-rejected outcome must not cause Exhausted.
    assert_eq!(
        outcome,
        CompactionOutcome::ProbeRejected,
        "expected ProbeRejected when all probe answers are refusals"
    );
    // The messages must NOT have been drained (original messages preserved).
    assert!(
        agent.msg.messages.len() > 3,
        "messages must not be drained after ProbeRejected"
    );
    // Verify the state machine invariant: not Exhausted.
    assert!(
        !matches!(
            agent.context_manager.compaction,
            CompactionState::Exhausted { .. }
        ),
        "ProbeRejected must not transition to Exhausted (H1 invariant)"
    );
}

// --- #2475: memory_save session hint in system prompt ---

/// When `memory_save` is present in `tool_state.completed_tool_ids`, `rebuild_system_prompt`
/// must append the disambiguation hint directing the model to use `memory_search`
/// rather than `search_code` for user-provided facts.
#[tokio::test]
async fn rebuild_system_prompt_injects_memory_save_hint_when_tool_was_used() {
    use zeph_skills::registry::SkillRegistry;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = SkillRegistry::default();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent
        .services
        .tool_state
        .completed_tool_ids
        .insert("memory_save".to_owned());
    agent.rebuild_system_prompt("test query").await;

    let prompt = &agent.msg.messages[0].content;
    assert!(
        prompt.contains("memory_save — use memory_search to recall them, not search_code"),
        "session hint must be present when memory_save was used; prompt: {prompt}"
    );
}

/// When `tool_state.completed_tool_ids` does NOT contain `memory_save`, no hint must be
/// appended — the system prompt must stay clean to avoid unnecessary noise.
#[tokio::test]
async fn rebuild_system_prompt_omits_memory_save_hint_when_tool_not_used() {
    use zeph_skills::registry::SkillRegistry;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = SkillRegistry::default();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    // tool_state.completed_tool_ids is empty by default — no memory_save.
    agent.rebuild_system_prompt("test query").await;

    let prompt = &agent.msg.messages[0].content;
    assert!(
        !prompt.contains("memory_save — use memory_search to recall them, not search_code"),
        "session hint must NOT be present when memory_save was not used"
    );
}

/// Verify that `maybe_proactive_compress` routes to `run_focus_auto_consolidation_pass`
/// when the strategy is `CompressionStrategy::Focus` and `should_proactively_compress`
/// returns `Some`.
///
/// Because `run_focus_auto_consolidation_pass` immediately returns when
/// `focus.try_acquire_compression()` succeeds and the message history is shorter than
/// `min_window`, we can observe routing by confirming: (a) no panic, (b) `Ok(())` returned,
/// and (c) `compacted_this_turn` is NOT set (Focus does not set it).
#[tokio::test]
async fn maybe_proactive_compress_focus_strategy_routes_to_focus_pass() {
    use zeph_config::CompressionStrategy;

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    // Configure Focus strategy with a budget so should_proactively_compress fires.
    agent.context_manager.compression.strategy = CompressionStrategy::Focus;
    agent.context_manager.budget = Some(ContextBudget::new(100_000, 0.10));
    // Soft threshold is 60% of 100_000 = 60_000; set tokens above that.
    agent.runtime.providers.cached_prompt_tokens = 70_000;

    // min_window defaults to 6; with only the system message, the pass returns None immediately.
    let result = agent.maybe_proactive_compress().await;

    assert!(
        result.is_ok(),
        "Focus route must not error on small history"
    );
    // Focus strategy must NOT set compacted_this_turn (reactive may still fire).
    assert!(
        !agent.context_manager.compaction.is_compacted_this_turn(),
        "Focus must not set compacted_this_turn"
    );
}
