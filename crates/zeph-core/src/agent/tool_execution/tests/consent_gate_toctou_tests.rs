// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end regression tests for the write-time memory-consent gate TOCTOU fix (#6558, #6569).
//!
//! Unlike `sanitize.rs`'s `consent_gate_dispatch_tests` (which unit-tests the pure building
//! blocks directly), these tests drive the real production entry point —
//! `Agent::handle_native_tool_calls` — with a genuine `MemoryToolExecutor` wired to the same
//! `MemoryConsentTrustSlot` the agent ratchets, exactly as `AgentBuilder::
//! with_memory_consent_trust_slot` wires it in `src/runner.rs`/`src/daemon.rs`/`src/acp.rs`/
//! `src/serve/agent_factory.rs`. A gate firing is asserted via `MockChannel::confirmed_prompts`
//! (`Channel::confirm` is only ever called by the confirmation phase when `MemoryToolExecutor`
//! returns `ToolError::ConfirmationRequired`), not by guessing at message text.

use std::sync::Arc;

use parking_lot::RwLock;
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{Message, MessageMetadata, Role, ToolUseRequest};
use zeph_memory::semantic::SemanticMemory;
use zeph_sanitizer::ContentTrustLevel;
use zeph_tools::CompositeExecutor;
use zeph_tools::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
use zeph_tools::registry::{InvocationHint, ToolDef};

use crate::agent::Agent;
use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};
use crate::agent::turn::TurnInput;
use crate::memory_tools::{MemoryConsentTrustSlot, MemoryToolExecutor};

/// Answers exactly one configured tool name with a fixed successful output; `Ok(None)` for
/// everything else, so it composes cleanly as the `first` half of a `CompositeExecutor` with a
/// real `MemoryToolExecutor` as `second` (unlike `MockToolExecutor`, which answers any call
/// regardless of `tool_id` — unusable here since both `web_scrape` and `memory_save` calls
/// must reach their own distinct handler).
struct SingleToolExecutor {
    tool_name: &'static str,
}

impl ToolExecutor for SingleToolExecutor {
    fn execute(
        &self,
        _response: &str,
    ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        std::future::ready(Ok(None))
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        vec![ToolDef {
            id: self.tool_name.into(),
            description: "test tool".into(),
            schema: schemars::Schema::default(),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
            server_id: None,
        }]
    }

    fn execute_tool_call(
        &self,
        call: &ToolCall,
    ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        let result = (|| {
            if call.tool_id.as_str() != self.tool_name {
                return Ok(None);
            }
            Ok(Some(ToolOutput {
                tool_name: self.tool_name.into(),
                summary: "scraped body".to_owned(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
                ..Default::default()
            }))
        })();
        std::future::ready(result)
    }

    zeph_tools::tool_executor_no_inner_defaults!();
}

async fn make_memory() -> SemanticMemory {
    SemanticMemory::with_sqlite_backend(
        ":memory:",
        AnyProvider::Mock(MockProvider::default()),
        "test-model",
        0.7,
        0.3,
    )
    .await
    .unwrap()
}

fn memory_save_call(id: &str, content: &str) -> ToolUseRequest {
    ToolUseRequest {
        id: id.to_owned(),
        name: "memory_save".to_owned().into(),
        input: serde_json::json!({ "content": content }),
    }
}

fn web_scrape_call(id: &str) -> ToolUseRequest {
    ToolUseRequest {
        id: id.to_owned(),
        name: "web_scrape".to_owned().into(),
        input: serde_json::json!({ "url": "https://example.com" }),
    }
}

/// Build an `Agent` whose tool executor handles `web_scrape` (fixed output) and `memory_save`
/// (real `MemoryToolExecutor`, consent-gated at `ExternalUntrusted` — the project default),
/// with the executor-side and agent-side halves of the gate sharing one `MemoryConsentTrustSlot`
/// exactly as production wiring requires (`MemoryToolExecutor::with_consent_gate`'s doc comment).
async fn make_agent() -> Agent<MockChannel> {
    let memory = make_memory().await;
    let sqlite = memory.sqlite().clone();
    let cid = sqlite.create_conversation().await.unwrap();
    let memory_executor = MemoryToolExecutor::new(Arc::new(memory), cid);
    // Placeholder slot — immediately overwritten below to share `agent`'s real slot, since
    // `Agent::new` does not accept one and always starts with its own fresh default.
    let placeholder_slot: MemoryConsentTrustSlot = Arc::new(RwLock::new(0u8));
    let memory_executor = memory_executor.with_consent_gate(
        Arc::clone(&placeholder_slot),
        ContentTrustLevel::ExternalUntrusted,
    );

    let executor = CompositeExecutor::new(
        SingleToolExecutor {
            tool_name: "web_scrape",
        },
        memory_executor,
    );

    let mut agent = Agent::new(
        mock_provider(vec![]),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        executor,
    );
    // `with_consent_gate` captured `placeholder_slot`; make the agent's own slot the *same*
    // `Arc` so `ratchet_memory_consent_trust_for_dispatch`'s writes are what the executor reads.
    agent.services.security.memory_consent_trust = placeholder_slot;
    agent
}

/// #6558 — cross-turn deferral bypass, closed.
///
/// Turn N dispatches `web_scrape`, tagging the resulting tool-result batch message
/// `ExternalUntrusted`. `begin_turn` then hard-resets the scalar slot to 0, simulating the
/// old (insufficient) turn boundary. Turn N+1 dispatches a lone `memory_save` — with no other
/// untrusted tool call in its own batch — referencing content "derived" from turn N's fetch,
/// exactly the "wait for your next reply" prompt-injection technique the issue describes. The
/// fix must still gate it, because the untrusted message is still sitting in `self.msg.messages`.
#[tokio::test]
async fn memory_save_gated_across_turn_boundary_when_untrusted_content_still_in_context() {
    let mut agent = make_agent().await;

    // Turn N: web_scrape only.
    agent
        .handle_native_tool_calls(None, &[web_scrape_call("t1")])
        .await
        .unwrap();
    assert_eq!(
        agent.msg.messages.last().unwrap().metadata.trust_level,
        Some(ContentTrustLevel::ExternalUntrusted as u8),
        "turn N's tool-result batch message must be tagged ExternalUntrusted"
    );

    // Turn boundary: begin_turn's floor reset (this alone is what #6558 exploited).
    let _turn = agent.begin_turn(TurnInput::new("turn 2".to_owned(), vec![]));
    assert_eq!(
        *agent.services.security.memory_consent_trust.read(),
        0,
        "begin_turn still floor-resets the slot — the fix must not depend on removing this"
    );

    // Turn N+1: memory_save alone, no other untrusted tool call in this batch.
    agent
        .handle_native_tool_calls(
            None,
            &[memory_save_call(
                "t2",
                "derived from the page fetched last turn",
            )],
        )
        .await
        .unwrap();

    let prompts = agent.channel.confirmed_prompts();
    assert!(
        prompts.iter().any(|p| p.contains("Save to memory")),
        "memory_save must have required confirmation across the turn boundary; \
         confirmed_prompts={prompts:?}"
    );
}

/// Negative control for #6558's fix: with an entirely clean context (no prior untrusted tool
/// output at all), a lone `memory_save` in turn N+1 must NOT be gated — proves the fix does not
/// over-trigger once the untrusted content genuinely never existed.
#[tokio::test]
async fn memory_save_not_gated_across_turn_boundary_with_clean_context() {
    let mut agent = make_agent().await;

    let _turn = agent.begin_turn(TurnInput::new("turn 1".to_owned(), vec![]));
    agent
        .handle_native_tool_calls(None, &[memory_save_call("t1", "a harmless fact")])
        .await
        .unwrap();

    let prompts = agent.channel.confirmed_prompts();
    assert!(
        prompts.is_empty(),
        "bare memory_save in a clean context must not require confirmation; \
         confirmed_prompts={prompts:?}"
    );
}

/// #6569 — same-tier / same-batch parallel-dispatch race, closed.
///
/// `web_scrape` and `memory_save` are dispatched together in ONE `handle_native_tool_calls`
/// call (the same shape the tier loop's DAG would put in a single tier, since neither call's
/// arguments reference the other's `tool_use_id`). Before the fix, `MemoryToolExecutor`'s gate
/// check ran during the tier's concurrent dispatch phase, before `web_scrape`'s output had been
/// through `sanitize_tool_output`'s ratchet — the slot still read `Trusted` at that instant.
/// The fix ratchets the slot from tool NAMES alone before any dispatch starts, so this is
/// deterministic (not a timing-dependent flaky race): the slot is already correct before either
/// tool call's future is even created.
#[tokio::test]
async fn memory_save_gated_when_dispatched_in_same_batch_as_web_scrape() {
    let mut agent = make_agent().await;

    agent
        .handle_native_tool_calls(
            None,
            &[
                web_scrape_call("t1"),
                memory_save_call("t2", "derived from the page fetched just now"),
            ],
        )
        .await
        .unwrap();

    let prompts = agent.channel.confirmed_prompts();
    assert!(
        prompts.iter().any(|p| p.contains("Save to memory")),
        "memory_save must be gated when batched with web_scrape in the same dispatch; \
         confirmed_prompts={prompts:?}"
    );
}

/// Regression guard: a lone `memory_save` call (no other tool in the batch, clean context)
/// must not self-gate — `memory_save`'s own not-yet-produced result must never contribute to
/// the pre-dispatch ratchet computed from the batch's tool names.
#[tokio::test]
async fn memory_save_alone_in_first_turn_is_not_gated() {
    let mut agent = make_agent().await;

    agent
        .handle_native_tool_calls(None, &[memory_save_call("t1", "a harmless fact")])
        .await
        .unwrap();

    let prompts = agent.channel.confirmed_prompts();
    assert!(
        prompts.is_empty(),
        "lone memory_save must not require confirmation on first dispatch; \
         confirmed_prompts={prompts:?}"
    );
}

// ── #6558 follow-up (S3): cross-process reload must still gate memory_save ────────────────
//
// Both tests below simulate a process restart (daemon/serve/ACP reattach): a FRESH `Agent`
// reloads conversation history from the SAME `SqliteStore` used by an earlier "process", then
// dispatches a lone `memory_save`. The gate must still fire because `load_history` (S1 fix)
// restores `MessageMetadata::trust_level` from the persisted DB column.

/// Simplest reload case: a plain provenance-tagged message (as `Agent::persist_message_with_
/// provenance` writes for a `web_scrape` batch) survives a reload and still gates.
#[tokio::test]
async fn memory_save_gated_after_process_restart_reload_from_db() {
    let memory = make_memory().await;
    let sqlite = memory.sqlite().clone();
    let cid = sqlite.create_conversation().await.unwrap();

    // Turn N (prior process): persist an untrusted tool-result message with provenance.
    sqlite
        .save_message_with_provenance(
            cid,
            "user",
            "scraped content",
            "[]",
            zeph_llm::provider::MessageVisibility::Both,
            Some("web_scrape"),
            Some("external_untrusted"),
        )
        .await
        .unwrap();

    // Simulated restart: reload history from the same store.
    let reloaded = sqlite.load_history(cid, 50).await.unwrap();
    assert!(
        reloaded
            .iter()
            .any(|m| m.metadata.trust_level == Some(ContentTrustLevel::ExternalUntrusted as u8)),
        "load_history must restore the persisted trust tag"
    );

    let memory_arc = Arc::new(memory);
    let placeholder_slot: MemoryConsentTrustSlot = Arc::new(RwLock::new(0u8));
    let memory_executor = MemoryToolExecutor::new(Arc::clone(&memory_arc), cid).with_consent_gate(
        Arc::clone(&placeholder_slot),
        ContentTrustLevel::ExternalUntrusted,
    );

    let mut agent = Agent::new(
        mock_provider(vec![]),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        memory_executor,
    );
    agent.services.security.memory_consent_trust = placeholder_slot;
    for m in reloaded {
        agent.msg.messages.push(m);
    }

    agent
        .handle_native_tool_calls(
            None,
            &[memory_save_call("t1", "derived from the reloaded content")],
        )
        .await
        .unwrap();

    let prompts = agent.channel.confirmed_prompts();
    assert!(
        prompts.iter().any(|p| p.contains("Save to memory")),
        "memory_save must be gated after a fresh agent reloads a previously-persisted \
         untrusted message from the DB; confirmed_prompts={prompts:?}"
    );
}

/// Full path: `compact_context` persists the compacted-away messages' worst-case trust tier
/// onto the summary row (S2 + S3 fixes composed), and a fresh agent reloading that summary
/// row after a simulated restart still gates a subsequent `memory_save`.
#[tokio::test]
#[allow(clippy::too_many_lines)] // single cohesive scenario: persist -> compact -> reload -> gate
async fn memory_save_gated_after_compaction_persist_and_reload() {
    let provider = mock_provider(vec!["compacted summary of untrusted content".to_owned()]);
    let memory =
        SemanticMemory::with_sqlite_backend(":memory:", provider.clone(), "test-model", 0.7, 0.3)
            .await
            .unwrap();
    let cid = memory.sqlite().create_conversation().await.unwrap();
    let sqlite = memory.sqlite().clone();

    sqlite
        .save_message(cid, "system", "system prompt")
        .await
        .unwrap();
    for i in 0..5 {
        sqlite
            .save_message(cid, "user", &format!("message {i}"))
            .await
            .unwrap();
    }

    let mut compactor_agent = Agent::new(
        provider,
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        MockToolExecutor::no_tools(),
    )
    .with_memory(Arc::new(memory), cid, 50, 5, 50)
    .with_context_budget(10_000, 0.20, 0.80, 2, 0);

    compactor_agent.msg.messages.push(Message {
        role: Role::User,
        content: "system prompt".to_owned(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });
    // Tag one in-memory message ExternalUntrusted, simulating a web_scrape-derived
    // tool-result batch that Agent::process_tool_result_batch tagged this turn.
    compactor_agent.msg.messages.push(Message {
        role: Role::User,
        content: "scraped content".to_owned(),
        parts: vec![],
        metadata: MessageMetadata {
            trust_level: Some(ContentTrustLevel::ExternalUntrusted as u8),
            ..MessageMetadata::default()
        },
    });
    for i in 0..10 {
        compactor_agent.msg.messages.push(Message {
            role: Role::User,
            content: format!("message {i}"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
    }

    let _ = compactor_agent.compact_context().await.unwrap();

    let memory_ref = compactor_agent
        .services
        .memory
        .persistence
        .memory
        .as_ref()
        .unwrap();
    let agent_visible = memory_ref
        .sqlite()
        .load_history_filtered(cid, 50, Some(true), None)
        .await
        .unwrap();
    let summary_row = agent_visible
        .iter()
        .find(|m| m.content.contains("compacted summary"))
        .expect("compaction must have inserted a summary row");
    assert_eq!(
        summary_row.metadata.trust_level,
        Some(ContentTrustLevel::ExternalUntrusted as u8),
        "persisted summary row must carry the compacted-away message's worst-case trust tier"
    );

    // Simulated restart: fresh agent reloads history (including the persisted summary row)
    // from the same store, then dispatches a lone memory_save.
    let reloaded = memory_ref.sqlite().load_history(cid, 50).await.unwrap();

    let placeholder_slot: MemoryConsentTrustSlot = Arc::new(RwLock::new(0u8));
    let memory_executor = MemoryToolExecutor::new(Arc::clone(memory_ref), cid).with_consent_gate(
        Arc::clone(&placeholder_slot),
        ContentTrustLevel::ExternalUntrusted,
    );
    let mut agent2 = Agent::new(
        mock_provider(vec![]),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        memory_executor,
    );
    agent2.services.security.memory_consent_trust = placeholder_slot;
    for m in reloaded {
        agent2.msg.messages.push(m);
    }

    agent2
        .handle_native_tool_calls(
            None,
            &[memory_save_call("t1", "derived from the reloaded summary")],
        )
        .await
        .unwrap();

    let prompts = agent2.channel.confirmed_prompts();
    assert!(
        prompts.iter().any(|p| p.contains("Save to memory")),
        "memory_save must be gated after reload from the persisted (compacted) summary row; \
         confirmed_prompts={prompts:?}"
    );
}
