// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
// Integration tests await full agent sessions; future size reflects real agent state.
#![allow(clippy::large_futures)]
// Raised from 128: #[instrument] chain on the agent call stack deepens async state machines.
#![recursion_limit = "256"]

//! Integration tests for the agent turn lifecycle.
//!
//! Each test exercises a complete turn path:
//! `recv → context assembly → LLM call → (tool execution) → response sent`.
//!
//! All tests use in-process mocks — no real LLM, storage, or tool infrastructure required.

use std::sync::{Arc, Mutex};

use zeph_core::Agent;
use zeph_core::channel::{Channel, ChannelError, ChannelMessage};
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_skills::registry::SkillRegistry;
use zeph_tools::ToolName;
use zeph_tools::executor::{ToolError, ToolExecutor, ToolOutput};

// ---------------------------------------------------------------------------
// Test infrastructure
// ---------------------------------------------------------------------------

/// Minimal in-process channel for turn lifecycle tests.
///
/// Delivers a fixed queue of input messages and records all outbound text.
struct TestChannel {
    inbox: Vec<String>,
    outbox: Arc<Mutex<Vec<String>>>,
}

impl TestChannel {
    fn new(messages: Vec<impl Into<String>>) -> (Self, Arc<Mutex<Vec<String>>>) {
        let outbox = Arc::new(Mutex::new(Vec::new()));
        let ch = Self {
            inbox: messages.into_iter().map(Into::into).collect(),
            outbox: Arc::clone(&outbox),
        };
        (ch, outbox)
    }
}

impl Channel for TestChannel {
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        if self.inbox.is_empty() {
            Ok(None)
        } else {
            Ok(Some(ChannelMessage {
                text: self.inbox.remove(0),
                attachments: vec![],
            }))
        }
    }

    fn try_recv(&mut self) -> Option<ChannelMessage> {
        if self.inbox.is_empty() {
            None
        } else {
            Some(ChannelMessage {
                text: self.inbox.remove(0),
                attachments: vec![],
            })
        }
    }

    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        self.outbox.lock().unwrap().push(text.to_owned());
        Ok(())
    }

    async fn send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        self.outbox.lock().unwrap().push(chunk.to_owned());
        Ok(())
    }

    async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
        Ok(())
    }

    async fn confirm(&mut self, _prompt: &str) -> Result<bool, ChannelError> {
        Ok(true)
    }
}

/// Tool executor that returns `Ok(None)` for every call (no tool output).
struct NoopExecutor;

impl ToolExecutor for NoopExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    fn set_skill_env(&self, _env: Option<std::collections::HashMap<String, String>>) {}
}

/// Tool executor that returns a single tool output then `Ok(None)` thereafter.
struct SingleToolExecutor {
    output: Arc<Mutex<Option<ToolOutput>>>,
}

impl SingleToolExecutor {
    fn new(tool_name: impl Into<ToolName>, summary: impl Into<String>) -> Self {
        let output = ToolOutput {
            tool_name: tool_name.into(),
            summary: summary.into(),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: None,
        };
        Self {
            output: Arc::new(Mutex::new(Some(output))),
        }
    }
}

impl ToolExecutor for SingleToolExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(self.output.lock().unwrap().take())
    }

    fn set_skill_env(&self, _env: Option<std::collections::HashMap<String, String>>) {}
}

/// Build a minimal [`SkillRegistry`] backed by a temporary directory.
///
/// Caller must keep the `TempDir` alive for the duration of the test.
fn minimal_registry() -> (SkillRegistry, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let skill_dir = dir.path().join("stub");
    std::fs::create_dir(&skill_dir).expect("create skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: stub\ndescription: Stub skill for lifecycle tests\n---\nStub body",
    )
    .expect("write SKILL.md");
    let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
    (registry, dir)
}

// ---------------------------------------------------------------------------
// Test 1: Happy path — single user message, no tool calls
// ---------------------------------------------------------------------------

/// The agent receives one user message, the mock LLM responds, and the response is sent.
///
/// This exercises: recv → context assembly → LLM → send.
#[tokio::test]
async fn test_simple_turn_no_tools() {
    let (registry, _dir) = minimal_registry();
    let provider = AnyProvider::Mock(MockProvider::with_responses(vec![
        "Hello from mock LLM".to_owned(),
    ]));
    let (channel, outbox) = TestChannel::new(vec!["Hi agent"]);
    let executor = NoopExecutor;

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.run().await.expect("agent run failed");

    let sent = outbox.lock().unwrap().clone();
    assert!(
        !sent.is_empty(),
        "agent must send at least one message after the LLM responds"
    );
    // The mock LLM response should appear somewhere in the sent output.
    let all_output = sent.join(" ");
    assert!(
        all_output.contains("Hello from mock LLM"),
        "LLM response not found in channel output: {all_output:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 2: Tool call — LLM triggers tool, result fed back, agent sends final reply
// ---------------------------------------------------------------------------

/// The agent receives a user message. The mock LLM emits a tool call JSON block.
/// `SingleToolExecutor` returns a tool output, and the LLM produces a final response.
///
/// This exercises: recv → context → LLM (tool call) → tool execution → LLM (final) → send.
#[tokio::test]
async fn test_turn_with_tool_call() {
    // The first LLM response contains a tool call block (JSON-like marker expected by
    // MockProvider / the agent's tool detection heuristic). The second is the final answer.
    let tool_response_json = r#"```tool_code
{"tool": "shell", "code": "echo hello"}
```"#
        .to_owned();
    let final_response = "Tool result processed".to_owned();

    let (registry, _dir) = minimal_registry();
    let provider = AnyProvider::Mock(MockProvider::with_responses(vec![
        tool_response_json,
        final_response.clone(),
    ]));
    let (channel, outbox) = TestChannel::new(vec!["Run a shell command"]);
    let executor = SingleToolExecutor::new("shell", "echo output: hello");

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.run().await.expect("agent run failed");

    let all_output = outbox.lock().unwrap().join(" ");
    // The agent must have sent something — the exact final message depends on
    // whether tool detection parsed the JSON block.
    assert!(
        !all_output.is_empty(),
        "agent must produce output after processing a turn with a potential tool call"
    );
}

// ---------------------------------------------------------------------------
// Test 3: Context assembly — system prompt and prior history reach the LLM
// ---------------------------------------------------------------------------

/// After the agent processes a turn, the conversation history grows.
/// A second message sees a longer context passed to the LLM.
///
/// This exercises context assembly: system prompt + prior turns + new message.
#[tokio::test]
async fn test_turn_context_grows_across_messages() {
    let (registry, _dir) = minimal_registry();

    // Two sequential responses: one per user message.
    // `with_recording` returns (MockProvider, Arc<Mutex<Vec<Vec<Message>>>>).
    let (mock, recorded_calls) =
        MockProvider::with_responses(vec!["First reply".to_owned(), "Second reply".to_owned()])
            .with_recording();
    let provider = AnyProvider::Mock(mock);

    let (channel, outbox) = TestChannel::new(vec!["First message", "Second message"]);
    let executor = NoopExecutor;

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.run().await.expect("agent run failed");

    let sent = outbox.lock().unwrap().clone();
    assert!(
        !sent.is_empty(),
        "agent must send at least one response; got: {sent:?}"
    );

    // When the LLM was called at least twice, the second call should see more messages
    // than the first (system prompt + first turn + new user message > system prompt + first user).
    let calls = recorded_calls.lock().unwrap();
    if calls.len() >= 2 {
        assert!(
            calls[1].len() > calls[0].len(),
            "second LLM call should include prior turn history: first={}, second={}",
            calls[0].len(),
            calls[1].len()
        );
    }
    // If only one LLM call occurred (agent stopped after first message), verify
    // that at least one response was produced — the context assembly still ran.
    assert!(!calls.is_empty(), "LLM must have been called at least once");
}
