// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared test infrastructure for `crate::agent` tests.
//!
//! Provides `MockChannel`, `MockToolExecutor`, `QuickTestAgent`, and helper
//! functions re-exported through `agent_tests` for cross-module visibility.

pub(crate) use std::sync::{Arc, Mutex};

type ToolOutputResult = Result<Option<ToolOutput>, ToolError>;
type EnvSnapshot = Option<std::collections::HashMap<String, String>>;

#[allow(unused_imports)]
pub(crate) use sqlx::prelude::*;
pub(crate) use tokio::sync::{Notify, mpsc, watch};
pub(crate) use zeph_llm::any::AnyProvider;
pub(crate) use zeph_llm::mock::MockProvider;
pub(crate) use zeph_llm::provider::{Message, MessageMetadata, Role};
pub(crate) use zeph_memory::semantic::SemanticMemory;
pub(crate) use zeph_skills::registry::SkillRegistry;
pub(crate) use zeph_skills::watcher::SkillEvent;
pub(crate) use zeph_tools::executor::{ToolError, ToolExecutor, ToolOutput};

pub(crate) use crate::agent::message_queue::{MAX_AUDIO_BYTES, MAX_IMAGE_BYTES, detect_image_mime};
pub(crate) use crate::agent::{
    Agent, TOOL_OUTPUT_SUFFIX, format_tool_output, recv_optional, shutdown_signal,
};
pub(crate) use crate::channel::{
    Attachment, AttachmentKind, Channel, ChannelMessage, ToolStartEvent,
};
pub(crate) use crate::config::{SecurityConfig, TimeoutConfig};
pub(crate) use crate::metrics::MetricsSnapshot;

/// Minimal test harness: an Agent wired with a `MockChannel` and `MockToolExecutor`.
/// Use `QuickTestAgent::minimal()` to get a ready-to-use agent for unit tests.
pub(crate) struct QuickTestAgent {
    pub(crate) agent: Agent<MockChannel>,
}

impl QuickTestAgent {
    /// Create a minimal agent with a single mock response and no tools.
    pub(crate) fn minimal(response: &str) -> Self {
        let provider = mock_provider(vec![response.to_owned()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        Self {
            agent: Agent::new(provider, channel, registry, None, 5, executor),
        }
    }

    /// Create a minimal agent with multiple mock responses and no tools.
    pub(crate) fn with_responses(responses: Vec<String>) -> Self {
        let provider = mock_provider(responses);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        Self {
            agent: Agent::new(provider, channel, registry, None, 5, executor),
        }
    }

    /// Return the messages sent to the channel so far.
    pub(crate) fn sent_messages(&self) -> Vec<String> {
        self.agent.channel.sent_messages()
    }
}

pub(crate) fn mock_provider(responses: Vec<String>) -> AnyProvider {
    AnyProvider::Mock(MockProvider::with_responses(responses))
}

pub(crate) fn mock_provider_streaming(responses: Vec<String>) -> AnyProvider {
    AnyProvider::Mock(MockProvider::with_responses(responses).with_streaming())
}

pub(crate) fn mock_provider_failing() -> AnyProvider {
    AnyProvider::Mock(MockProvider::failing())
}

pub(crate) fn mock_provider_with_models(
    responses: Vec<String>,
    models: Vec<zeph_llm::model_cache::RemoteModelInfo>,
) -> AnyProvider {
    AnyProvider::Mock(MockProvider::with_responses(responses).with_models(models))
}

pub(crate) struct MockChannel {
    pub(crate) messages: Arc<Mutex<Vec<String>>>,
    pub(crate) sent: Arc<Mutex<Vec<String>>>,
    pub(crate) chunks: Arc<Mutex<Vec<String>>>,
    pub(crate) confirmations: Arc<Mutex<Vec<bool>>>,
    pub(crate) statuses: Arc<Mutex<Vec<String>>>,
    pub(crate) tool_starts: Arc<Mutex<Vec<ToolStartEvent>>>,
    pub(crate) exit_supported: bool,
}

impl MockChannel {
    pub(crate) fn new(messages: Vec<String>) -> Self {
        Self {
            messages: Arc::new(Mutex::new(messages)),
            sent: Arc::new(Mutex::new(Vec::new())),
            chunks: Arc::new(Mutex::new(Vec::new())),
            confirmations: Arc::new(Mutex::new(Vec::new())),
            statuses: Arc::new(Mutex::new(Vec::new())),
            tool_starts: Arc::new(Mutex::new(Vec::new())),
            exit_supported: true,
        }
    }

    pub(crate) fn without_exit_support(mut self) -> Self {
        self.exit_supported = false;
        self
    }

    pub(crate) fn with_confirmations(mut self, confirmations: Vec<bool>) -> Self {
        self.confirmations = Arc::new(Mutex::new(confirmations));
        self
    }

    pub(crate) fn sent_messages(&self) -> Vec<String> {
        self.sent.lock().unwrap().clone()
    }
}

impl Channel for MockChannel {
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, crate::channel::ChannelError> {
        let mut msgs = self.messages.lock().unwrap();
        if msgs.is_empty() {
            Ok(None)
        } else {
            Ok(Some(ChannelMessage {
                text: msgs.remove(0),
                attachments: vec![],
                is_guest_context: false,
                is_from_bot: false,
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
                is_guest_context: false,
                is_from_bot: false,
            })
        }
    }

    async fn send(&mut self, text: &str) -> Result<(), crate::channel::ChannelError> {
        self.sent.lock().unwrap().push(text.to_string());
        Ok(())
    }

    async fn send_chunk(&mut self, chunk: &str) -> Result<(), crate::channel::ChannelError> {
        self.chunks.lock().unwrap().push(chunk.to_string());
        Ok(())
    }

    async fn flush_chunks(&mut self) -> Result<(), crate::channel::ChannelError> {
        Ok(())
    }

    async fn send_status(&mut self, text: &str) -> Result<(), crate::channel::ChannelError> {
        self.statuses.lock().unwrap().push(text.to_string());
        Ok(())
    }

    async fn send_tool_start(
        &mut self,
        event: ToolStartEvent,
    ) -> Result<(), crate::channel::ChannelError> {
        self.tool_starts.lock().unwrap().push(event);
        Ok(())
    }

    async fn confirm(&mut self, _prompt: &str) -> Result<bool, crate::channel::ChannelError> {
        let mut confs = self.confirmations.lock().unwrap();
        Ok(if confs.is_empty() {
            true
        } else {
            confs.remove(0)
        })
    }

    fn supports_exit(&self) -> bool {
        self.exit_supported
    }
}

pub(crate) struct MockToolExecutor {
    outputs: Arc<Mutex<Vec<ToolOutputResult>>>,
    pub(crate) captured_env: Arc<Mutex<Vec<EnvSnapshot>>>,
}

impl MockToolExecutor {
    pub(crate) fn new(outputs: Vec<ToolOutputResult>) -> Self {
        Self {
            outputs: Arc::new(Mutex::new(outputs)),
            captured_env: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn no_tools() -> Self {
        Self::new(vec![Ok(None)])
    }
}

impl ToolExecutor for MockToolExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        let mut outputs = self.outputs.lock().unwrap();
        if outputs.is_empty() {
            Ok(None)
        } else {
            outputs.remove(0)
        }
    }

    async fn execute_tool_call(
        &self,
        _call: &zeph_tools::executor::ToolCall,
    ) -> Result<Option<ToolOutput>, ToolError> {
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

pub(crate) fn create_test_registry() -> SkillRegistry {
    let temp_dir = tempfile::tempdir().unwrap();
    let skill_dir = temp_dir.path().join("test-skill");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: test-skill\ndescription: A test skill\n---\nTest skill body",
    )
    .unwrap();
    SkillRegistry::load(&[temp_dir.path().to_path_buf()])
}
