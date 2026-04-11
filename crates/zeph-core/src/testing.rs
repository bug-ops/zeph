// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reusable test helpers: `MockChannel`, `MockToolExecutor`, and `AgentTestHarness`.
//!
//! Gated behind `#[cfg(test)]` — only compiled in test builds.

use std::sync::{Arc, Mutex};

use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_skills::registry::SkillRegistry;
use zeph_tools::executor::{ToolError, ToolOutput};

use crate::agent::Agent;
use crate::channel::{Channel, ChannelError, ChannelMessage};

// ---------------------------------------------------------------------------
// MockChannel
// ---------------------------------------------------------------------------

/// In-memory [`Channel`] implementation for unit tests.
///
/// Pre-loads a queue of input messages returned from [`recv`](Channel::recv).
/// Captures all outbound text in a `Vec<String>` accessible via
/// [`sent_messages`](MockChannel::sent_messages).
pub struct MockChannel {
    messages: Arc<Mutex<Vec<String>>>,
    sent: Arc<Mutex<Vec<String>>>,
    chunks: Arc<Mutex<Vec<String>>>,
    confirmations: Arc<Mutex<Vec<bool>>>,
}

impl MockChannel {
    /// Create a channel that will serve `messages` in order and auto-confirm all
    /// prompts.
    #[must_use]
    pub fn new(messages: Vec<impl Into<String>>) -> Self {
        Self {
            messages: Arc::new(Mutex::new(messages.into_iter().map(Into::into).collect())),
            sent: Arc::new(Mutex::new(Vec::new())),
            chunks: Arc::new(Mutex::new(Vec::new())),
            confirmations: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Override the auto-confirm behaviour with an explicit sequence of responses.
    #[must_use]
    pub fn with_confirmations(mut self, confirmations: Vec<bool>) -> Self {
        self.confirmations = Arc::new(Mutex::new(confirmations));
        self
    }

    /// Return all full messages sent via [`Channel::send`].
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn sent_messages(&self) -> Vec<String> {
        self.sent.lock().unwrap().clone()
    }

    /// Return all chunks sent via [`Channel::send_chunk`].
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn sent_chunks(&self) -> Vec<String> {
        self.chunks.lock().unwrap().clone()
    }
}

impl Channel for MockChannel {
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        let mut msgs = self.messages.lock().unwrap();
        if msgs.is_empty() {
            Ok(None)
        } else {
            Ok(Some(ChannelMessage {
                text: msgs.remove(0),
                attachments: vec![],
            }))
        }
    }

    fn try_recv(&mut self) -> Option<ChannelMessage> {
        let mut msgs = self.messages.lock().unwrap();
        if msgs.is_empty() {
            None
        } else {
            Some(ChannelMessage {
                text: msgs.remove(0),
                attachments: vec![],
            })
        }
    }

    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        self.sent.lock().unwrap().push(text.to_string());
        Ok(())
    }

    async fn send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        self.chunks.lock().unwrap().push(chunk.to_string());
        Ok(())
    }

    async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
        Ok(())
    }

    async fn confirm(&mut self, _prompt: &str) -> Result<bool, ChannelError> {
        let mut confs = self.confirmations.lock().unwrap();
        Ok(if confs.is_empty() {
            true
        } else {
            confs.remove(0)
        })
    }
}

// ---------------------------------------------------------------------------
// MockToolExecutor
// ---------------------------------------------------------------------------

/// Configurable [`ToolExecutor`](zeph_tools::executor::ToolExecutor) for unit tests.
///
/// Returns pre-configured responses in order. When the queue is exhausted,
/// subsequent calls return `Ok(None)`.
type OutputQueue = Arc<Mutex<Vec<Result<Option<ToolOutput>, ToolError>>>>;
type EnvCapture = Arc<Mutex<Vec<Option<std::collections::HashMap<String, String>>>>>;

pub struct MockToolExecutor {
    outputs: OutputQueue,
    /// Records each `set_skill_env` call; inspect in assertions.
    pub captured_env: EnvCapture,
}

impl MockToolExecutor {
    /// Create an executor that returns `outputs` in order.
    #[must_use]
    pub fn new(outputs: Vec<Result<Option<ToolOutput>, ToolError>>) -> Self {
        Self {
            outputs: Arc::new(Mutex::new(outputs)),
            captured_env: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Executor that returns `Ok(None)` for every call — simulates no tool invocations.
    #[must_use]
    pub fn no_tools() -> Self {
        Self::new(vec![Ok(None)])
    }

    /// Executor that returns a single successful tool output with the given summary.
    #[must_use]
    pub fn with_output(
        tool_name: impl Into<zeph_tools::ToolName>,
        summary: impl Into<String>,
    ) -> Self {
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
        Self::new(vec![Ok(Some(output))])
    }
}

impl zeph_tools::executor::ToolExecutor for MockToolExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        let mut outputs = self.outputs.lock().unwrap();
        if outputs.is_empty() {
            Ok(None)
        } else {
            outputs.remove(0)
        }
    }

    fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        self.captured_env.lock().unwrap().push(env);
    }
}

// ---------------------------------------------------------------------------
// AgentTestHarness
// ---------------------------------------------------------------------------

/// Builder that wires up a fully functional [`Agent`] for integration tests.
///
/// Uses [`MockProvider`], [`MockChannel`], and [`MockToolExecutor`] so tests
/// do not need real LLM, storage, or tool infrastructure.
///
/// # Example
///
/// ```rust,ignore
/// let harness = AgentTestHarness::new()
///     .with_responses(vec!["hello from mock".into()])
///     .with_messages(vec!["hi".into()])
///     .build();
///
/// let sent = harness.channel_ref().sent_messages(); // inspect after run
/// ```
pub struct AgentTestHarness {
    responses: Vec<String>,
    messages: Vec<String>,
    registry: Option<SkillRegistry>,
    max_active_skills: usize,
    tool_outputs: Vec<Result<Option<ToolOutput>, ToolError>>,
}

impl Default for AgentTestHarness {
    fn default() -> Self {
        Self {
            responses: vec!["mock response".into()],
            messages: vec![],
            registry: None,
            max_active_skills: 5,
            tool_outputs: vec![Ok(None)],
        }
    }
}

impl AgentTestHarness {
    /// Create a new harness with sensible defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the LLM responses the mock provider will return (in order).
    #[must_use]
    pub fn with_responses(mut self, responses: Vec<String>) -> Self {
        self.responses = responses;
        self
    }

    /// Set the input messages the [`MockChannel`] will deliver to the agent.
    #[must_use]
    pub fn with_messages(mut self, messages: Vec<String>) -> Self {
        self.messages = messages;
        self
    }

    /// Override the skill registry (defaults to an empty registry with one stub skill).
    #[must_use]
    pub fn with_registry(mut self, registry: SkillRegistry) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Set the tool executor outputs returned in order.
    #[must_use]
    pub fn with_tool_outputs(
        mut self,
        outputs: Vec<Result<Option<ToolOutput>, ToolError>>,
    ) -> Self {
        self.tool_outputs = outputs;
        self
    }

    /// Build the [`Agent`]. The `MockChannel` is moved into the agent; use
    /// the returned channel handle to inspect sent messages after the run.
    ///
    /// When no registry was set via [`with_registry`](Self::with_registry), a
    /// temporary directory is created and its guard is included in the return
    /// value.  The caller must hold the `Option<TempDir>` for the entire test —
    /// dropping it early removes the backing skill files.
    #[must_use]
    pub fn build(self) -> (Agent<MockChannel>, ChannelHandle, Option<tempfile::TempDir>) {
        let (registry, tempdir) = if let Some(r) = self.registry {
            (r, None)
        } else {
            let (r, d) = empty_registry();
            (r, Some(d))
        };
        let provider = AnyProvider::Mock(MockProvider::with_responses(self.responses));
        let channel = MockChannel::new(self.messages);
        let sent = Arc::clone(&channel.sent);
        let chunks = Arc::clone(&channel.chunks);
        let executor = MockToolExecutor::new(self.tool_outputs);
        let agent = Agent::new(
            provider,
            channel,
            registry,
            None,
            self.max_active_skills,
            executor,
        );
        let handle = ChannelHandle { sent, chunks };
        (agent, handle, tempdir)
    }
}

/// Read-only handle for inspecting [`MockChannel`] output after the agent run.
pub struct ChannelHandle {
    sent: Arc<Mutex<Vec<String>>>,
    chunks: Arc<Mutex<Vec<String>>>,
}

impl ChannelHandle {
    /// Return all full messages sent by the agent.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn sent_messages(&self) -> Vec<String> {
        self.sent.lock().unwrap().clone()
    }

    /// Return all streaming chunks sent by the agent.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn sent_chunks(&self) -> Vec<String> {
        self.chunks.lock().unwrap().clone()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build an empty [`SkillRegistry`] backed by a temporary directory.
///
/// Returns both the registry and the [`tempfile::TempDir`] guard. The caller
/// must hold the `TempDir` for the lifetime of the registry — dropping it
/// early removes the backing directory, invalidating any paths stored inside
/// the registry.
///
/// The registry contains a single stub skill so prompts are not empty.
///
/// # Panics
///
/// Panics if the temporary directory or stub skill file cannot be created.
#[must_use]
pub fn empty_registry() -> (SkillRegistry, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let skill_dir = dir.path().join("stub");
    std::fs::create_dir(&skill_dir).expect("create skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: stub\ndescription: Stub skill for testing\n---\nStub body",
    )
    .expect("write SKILL.md");
    let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
    (registry, dir)
}

/// Build a [`MockProvider`]-backed [`AnyProvider`] that returns `responses` in order.
#[must_use]
pub fn mock_provider(responses: Vec<String>) -> AnyProvider {
    AnyProvider::Mock(MockProvider::with_responses(responses))
}

/// Build a failing [`AnyProvider`] that returns an error on every chat call.
#[must_use]
pub fn failing_provider() -> AnyProvider {
    AnyProvider::Mock(MockProvider::failing())
}
