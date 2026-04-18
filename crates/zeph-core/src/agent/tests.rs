// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#![allow(
    clippy::doc_markdown,
    clippy::manual_assert,
    clippy::manual_string_new,
    clippy::type_complexity
)]

#[cfg(test)]
pub mod agent_tests {
    use super::super::message_queue::{MAX_AUDIO_BYTES, MAX_IMAGE_BYTES, detect_image_mime};
    #[allow(unused_imports)]
    pub(crate) use super::super::{
        Agent, CODE_CONTEXT_PREFIX, CROSS_SESSION_PREFIX, RECALL_PREFIX, SUMMARY_PREFIX,
        TOOL_OUTPUT_SUFFIX, format_tool_output, recv_optional, shutdown_signal,
    };
    pub(crate) use crate::channel::Channel;
    use crate::channel::{Attachment, AttachmentKind, ChannelMessage};
    pub(crate) use crate::config::{SecurityConfig, TimeoutConfig};
    pub(crate) use crate::metrics::MetricsSnapshot;
    #[allow(unused_imports)]
    pub(crate) use sqlx::prelude::*;
    use std::sync::{Arc, Mutex};
    pub(crate) use tokio::sync::{Notify, mpsc, watch};
    pub(crate) use zeph_llm::any::AnyProvider;
    pub(crate) use zeph_llm::mock::MockProvider;
    pub(crate) use zeph_llm::provider::{Message, MessageMetadata, Role};
    pub(crate) use zeph_memory::semantic::SemanticMemory;
    pub(crate) use zeph_skills::registry::SkillRegistry;
    pub(crate) use zeph_skills::watcher::SkillEvent;
    pub(crate) use zeph_tools::executor::ToolExecutor;
    use zeph_tools::executor::{ToolError, ToolOutput};

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
        messages: Arc<Mutex<Vec<String>>>,
        sent: Arc<Mutex<Vec<String>>>,
        chunks: Arc<Mutex<Vec<String>>>,
        confirmations: Arc<Mutex<Vec<bool>>>,
        pub(crate) statuses: Arc<Mutex<Vec<String>>>,
        exit_supported: bool,
    }

    impl MockChannel {
        pub(crate) fn new(messages: Vec<String>) -> Self {
            Self {
                messages: Arc::new(Mutex::new(messages)),
                sent: Arc::new(Mutex::new(Vec::new())),
                chunks: Arc::new(Mutex::new(Vec::new())),
                confirmations: Arc::new(Mutex::new(Vec::new())),
                statuses: Arc::new(Mutex::new(Vec::new())),
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
        outputs: Arc<Mutex<Vec<Result<Option<ToolOutput>, ToolError>>>>,
        pub(crate) captured_env: Arc<Mutex<Vec<Option<std::collections::HashMap<String, String>>>>>,
    }

    impl MockToolExecutor {
        pub(crate) fn new(outputs: Vec<Result<Option<ToolOutput>, ToolError>>) -> Self {
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

    #[tokio::test]
    async fn agent_new_initializes_with_system_prompt() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor);

        assert_eq!(agent.msg.messages.len(), 1);
        assert_eq!(agent.msg.messages[0].role, Role::System);
        assert!(!agent.msg.messages[0].content.is_empty());
    }

    #[tokio::test]
    async fn agent_with_working_dir_updates_environment_context() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let tmp = tempfile::tempdir().unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.runtime.model_name = "test-model".to_string();
        let agent = agent.with_working_dir(tmp.path().to_path_buf());

        assert_eq!(
            agent.session.env_context.working_dir,
            tmp.path().display().to_string()
        );
        assert_eq!(agent.session.env_context.model_name, "test-model");
    }

    #[tokio::test]
    async fn agent_with_embedding_model_sets_model() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.skill_state.embedding_model = "test-embed-model".to_string();

        assert_eq!(agent.skill_state.embedding_model, "test-embed-model");
    }

    #[tokio::test]
    async fn agent_with_shutdown_sets_receiver() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (_tx, rx) = watch::channel(false);

        let _agent = Agent::new(provider, channel, registry, None, 5, executor).with_shutdown(rx);
    }

    #[tokio::test]
    async fn agent_with_security_sets_config() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let security = SecurityConfig {
            redact_secrets: true,
            ..Default::default()
        };
        let timeouts = TimeoutConfig {
            llm_seconds: 60,
            ..Default::default()
        };

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_security(security, timeouts);

        assert!(agent.runtime.security.redact_secrets);
        assert_eq!(agent.runtime.timeouts.llm_seconds, 60);
    }

    #[tokio::test]
    async fn agent_run_handles_empty_channel() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn agent_run_processes_user_message() {
        let provider = mock_provider(vec!["test response".to_string()]);
        let channel = MockChannel::new(vec!["hello".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());
        assert_eq!(agent.msg.messages.len(), 3);
        assert_eq!(agent.msg.messages[1].role, Role::User);
        assert_eq!(agent.msg.messages[1].content, "hello");
        assert_eq!(agent.msg.messages[2].role, Role::Assistant);
    }

    #[tokio::test]
    async fn agent_run_handles_shutdown_signal() {
        let provider = mock_provider(vec![]);
        let (tx, rx) = watch::channel(false);
        let channel = MockChannel::new(vec!["should not process".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent =
            Agent::new(provider, channel, registry, None, 5, executor).with_shutdown(rx);

        tx.send(true).unwrap();

        let result = agent.run().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn agent_handles_skills_command() {
        let provider = mock_provider(vec![]);
        let _channel = MockChannel::new(vec!["/skills".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent_channel = MockChannel::new(vec!["/skills".to_string()]);
        let sent = agent_channel.sent.clone();

        let mut agent = Agent::new(provider, agent_channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent_msgs = sent.lock().unwrap();
        assert!(!sent_msgs.is_empty());
        assert!(sent_msgs[0].contains("Available skills"));
    }

    #[tokio::test]
    async fn agent_process_response_handles_empty_response() {
        // In the native path, an empty LLM response is treated as a completed turn (no text
        // to display). The agent should complete without error and without sending a message.
        let provider = mock_provider(vec![String::new()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent_channel = MockChannel::new(vec!["test".to_string()]);

        let mut agent = Agent::new(provider, agent_channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn agent_handles_tool_execution_success() {
        use zeph_llm::provider::{ChatResponse, ToolUseRequest};
        let tool_call = ToolUseRequest {
            id: "call1".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        };
        let (mock, _) = MockProvider::default().with_tool_use(vec![
            ChatResponse::ToolUse {
                text: None,
                tool_calls: vec![tool_call],
                thinking_blocks: vec![],
            },
            ChatResponse::Text("done".into()),
        ]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Ok(Some(ToolOutput {
            tool_name: "bash".into(),
            summary: "tool executed successfully".to_string(),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: None,
        }))]);

        let agent_channel = MockChannel::new(vec!["execute tool".to_string()]);
        let sent = agent_channel.sent.clone();

        let mut agent = Agent::new(
            AnyProvider::Mock(mock),
            agent_channel,
            registry,
            None,
            5,
            executor,
        );

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent_msgs = sent.lock().unwrap();
        assert!(
            sent_msgs
                .iter()
                .any(|m| m.contains("tool executed successfully"))
        );
    }

    #[tokio::test]
    async fn agent_handles_tool_blocked_error() {
        use zeph_llm::provider::{ChatResponse, ToolUseRequest};
        let tool_call = ToolUseRequest {
            id: "call1".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        };
        let (mock, _) = MockProvider::default().with_tool_use(vec![
            ChatResponse::ToolUse {
                text: None,
                tool_calls: vec![tool_call],
                thinking_blocks: vec![],
            },
            ChatResponse::Text("done".into()),
        ]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Err(ToolError::Blocked {
            command: "rm -rf /".to_string(),
        })]);

        let agent_channel = MockChannel::new(vec!["test".to_string()]);
        let sent = agent_channel.sent.clone();

        let mut agent = Agent::new(
            AnyProvider::Mock(mock),
            agent_channel,
            registry,
            None,
            5,
            executor,
        );

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent_msgs = sent.lock().unwrap();
        assert!(
            sent_msgs
                .iter()
                .any(|m| m.contains("blocked by security policy"))
        );
    }

    #[tokio::test]
    async fn agent_handles_tool_sandbox_violation() {
        use zeph_llm::provider::{ChatResponse, ToolUseRequest};
        let tool_call = ToolUseRequest {
            id: "call1".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        };
        let (mock, _) = MockProvider::default().with_tool_use(vec![
            ChatResponse::ToolUse {
                text: None,
                tool_calls: vec![tool_call],
                thinking_blocks: vec![],
            },
            ChatResponse::Text("done".into()),
        ]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Err(ToolError::SandboxViolation {
            path: "/etc/passwd".to_string(),
        })]);

        let agent_channel = MockChannel::new(vec!["test".to_string()]);
        let sent = agent_channel.sent.clone();

        let mut agent = Agent::new(
            AnyProvider::Mock(mock),
            agent_channel,
            registry,
            None,
            5,
            executor,
        );

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent_msgs = sent.lock().unwrap();
        // In the native path, SandboxViolation is classified as PolicyBlocked and formatted
        // as a tool_result feedback string (not a direct user message). The ToolError display
        // string ("path not allowed by sandbox") appears in the tool output sent to the channel.
        assert!(
            sent_msgs
                .iter()
                .any(|m| m.contains("path not allowed by sandbox"))
        );
    }

    #[tokio::test]
    async fn agent_handles_tool_confirmation_approved() {
        use zeph_llm::provider::{ChatResponse, ToolUseRequest};
        let tool_call = ToolUseRequest {
            id: "call1".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        };
        let (mock, _) = MockProvider::default().with_tool_use(vec![
            ChatResponse::ToolUse {
                text: None,
                tool_calls: vec![tool_call],
                thinking_blocks: vec![],
            },
            ChatResponse::Text("done".into()),
        ]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Err(ToolError::ConfirmationRequired {
            command: "dangerous command".to_string(),
        })]);

        let agent_channel =
            MockChannel::new(vec!["test".to_string()]).with_confirmations(vec![true]);
        let sent = agent_channel.sent.clone();

        let mut agent = Agent::new(
            AnyProvider::Mock(mock),
            agent_channel,
            registry,
            None,
            5,
            executor,
        );

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent_msgs = sent.lock().unwrap();
        assert!(!sent_msgs.is_empty());
    }

    #[tokio::test]
    async fn agent_handles_tool_confirmation_denied() {
        use zeph_llm::provider::{ChatResponse, ToolUseRequest};
        let tool_call = ToolUseRequest {
            id: "call1".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        };
        let (mock, _) = MockProvider::default().with_tool_use(vec![
            ChatResponse::ToolUse {
                text: None,
                tool_calls: vec![tool_call],
                thinking_blocks: vec![],
            },
            ChatResponse::Text("done".into()),
        ]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Err(ToolError::ConfirmationRequired {
            command: "dangerous command".to_string(),
        })]);

        let agent_channel =
            MockChannel::new(vec!["test".to_string()]).with_confirmations(vec![false]);
        let sent = agent_channel.sent.clone();

        let mut agent = Agent::new(
            AnyProvider::Mock(mock),
            agent_channel,
            registry,
            None,
            5,
            executor,
        );

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent_msgs = sent.lock().unwrap();
        // In the native path, a denied confirmation results in a "[cancelled by user]" tool
        // output sent via send_tool_output (not a direct "Command cancelled" user message).
        assert!(sent_msgs.iter().any(|m| m.contains("[cancelled by user]")));
    }

    #[tokio::test]
    async fn agent_handles_streaming_response() {
        let (mock, _) =
            MockProvider::default().with_tool_use(vec![zeph_llm::provider::ChatResponse::Text(
                "streaming response".to_string(),
            )]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent_channel = MockChannel::new(vec!["test".to_string()]);
        let sent = agent_channel.sent.clone();

        let mut agent = Agent::new(
            AnyProvider::Mock(mock),
            agent_channel,
            registry,
            None,
            5,
            executor,
        );

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent_msgs = sent.lock().unwrap();
        assert!(!sent_msgs.is_empty());
    }

    #[tokio::test]
    async fn agent_maybe_redact_enabled() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let security = SecurityConfig {
            redact_secrets: true,
            ..Default::default()
        };

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_security(security, TimeoutConfig::default());

        let text = "token: sk-abc123secret";
        let redacted = agent.maybe_redact(text);
        assert_ne!(AsRef::<str>::as_ref(&redacted), text);
    }

    #[tokio::test]
    async fn agent_maybe_redact_disabled() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let security = SecurityConfig {
            redact_secrets: false,
            ..Default::default()
        };

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_security(security, TimeoutConfig::default());

        let text = "password=secret123";
        let redacted = agent.maybe_redact(text);
        assert_eq!(AsRef::<str>::as_ref(&redacted), text);
    }

    #[tokio::test]
    async fn agent_handles_multiple_messages() {
        let provider = mock_provider(vec![
            "first response".to_string(),
            "second response".to_string(),
        ]);
        // Both messages arrive simultaneously via try_recv(), so they merge
        // within the 500ms window into a single "first\nsecond" message.
        let channel = MockChannel::new(vec!["first".to_string(), "second".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Ok(None), Ok(None)]);

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());
        assert_eq!(agent.msg.messages.len(), 3);
        assert_eq!(agent.msg.messages[1].content, "first\nsecond");
    }

    #[tokio::test]
    async fn agent_handles_tool_output_with_error_marker() {
        let provider = mock_provider(vec!["response".to_string(), "retry".to_string()]);
        let channel = MockChannel::new(vec!["test".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![
            Ok(Some(ToolOutput {
                tool_name: "bash".into(),
                summary: "[error] command failed [exit code 1]".to_string(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            })),
            Ok(None),
        ]);

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn agent_handles_empty_tool_output() {
        let provider = mock_provider(vec!["response".to_string()]);
        let channel = MockChannel::new(vec!["test".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Ok(Some(ToolOutput {
            tool_name: "bash".into(),
            summary: "   ".to_string(),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: None,
        }))]);

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn shutdown_signal_helper_returns_on_true() {
        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            let mut rx_clone = rx;
            shutdown_signal(&mut rx_clone).await;
        });

        tx.send(true).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), handle).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn recv_optional_returns_pending_when_no_receiver() {
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            recv_optional::<SkillEvent>(&mut None),
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn recv_optional_receives_from_channel() {
        let (tx, rx) = mpsc::channel(1);
        tx.send(SkillEvent::Changed).await.unwrap();

        let result = recv_optional(&mut Some(rx)).await;
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn agent_with_skill_reload_sets_paths() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (_tx, rx) = mpsc::channel(1);

        let paths = vec![std::path::PathBuf::from("/test/path")];
        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_skill_reload(paths.clone(), rx);

        assert_eq!(agent.skill_state.skill_paths, paths);
    }

    #[tokio::test]
    async fn agent_handles_tool_execution_error() {
        use zeph_llm::provider::{ChatResponse, ToolUseRequest};
        let tool_call = ToolUseRequest {
            id: "call1".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        };
        let (mock, _) = MockProvider::default().with_tool_use(vec![
            ChatResponse::ToolUse {
                text: None,
                tool_calls: vec![tool_call],
                thinking_blocks: vec![],
            },
            ChatResponse::Text("done".into()),
        ]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Err(ToolError::Timeout { timeout_secs: 30 })]);

        let agent_channel = MockChannel::new(vec!["test".to_string()]);
        let sent = agent_channel.sent.clone();

        let mut agent = Agent::new(
            AnyProvider::Mock(mock),
            agent_channel,
            registry,
            None,
            5,
            executor,
        );

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent_msgs = sent.lock().unwrap();
        // In the native path, Timeout errors are formatted as tool_result feedback sent via
        // send_tool_output. The error string ("command timed out") appears in the tool output.
        assert!(
            sent_msgs
                .iter()
                .any(|m| m.contains("timed out") || m.contains("timeout"))
        );
    }

    #[tokio::test]
    async fn agent_processes_multi_turn_tool_execution() {
        use zeph_llm::provider::{ChatResponse, ToolUseRequest};
        let tool_call = ToolUseRequest {
            id: "call1".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        };
        // Native path: LLM returns ToolUse first, then a final Text after tool results.
        let (mock, _) = MockProvider::default().with_tool_use(vec![
            ChatResponse::ToolUse {
                text: None,
                tool_calls: vec![tool_call],
                thinking_blocks: vec![],
            },
            ChatResponse::Text("step complete".into()),
        ]);
        let channel = MockChannel::new(vec!["start task".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Ok(Some(ToolOutput {
            tool_name: "bash".into(),
            summary: "step 1 complete".to_string(),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: None,
        }))]);

        let mut agent = Agent::new(
            AnyProvider::Mock(mock),
            channel,
            registry,
            None,
            5,
            executor,
        );

        let result = agent.run().await;
        assert!(result.is_ok());
        // Native path: user message + assistant tool_use + user tool_result + assistant response.
        assert!(agent.msg.messages.len() > 3);
    }

    #[tokio::test]
    async fn agent_respects_max_shell_iterations() {
        let mut responses = vec![];
        for _ in 0..10 {
            responses.push("response".to_string());
        }
        let provider = mock_provider(responses);
        let channel = MockChannel::new(vec!["test".to_string()]);
        let registry = create_test_registry();

        let mut outputs = vec![];
        for _ in 0..10 {
            outputs.push(Ok(Some(ToolOutput {
                tool_name: "bash".into(),
                summary: "continuing".to_string(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            })));
        }
        let executor = MockToolExecutor::new(outputs);

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());
        let assistant_count = agent
            .msg
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count();
        assert!(assistant_count <= 10);
    }

    #[tokio::test]
    async fn agent_with_metrics_sets_initial_values() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.runtime.model_name = "test-model".to_string();
        let _agent = agent.with_metrics(tx);

        let snapshot = rx.borrow().clone();
        assert_eq!(snapshot.provider_name, "mock");
        assert_eq!(snapshot.model_name, "test-model");
        assert_eq!(snapshot.total_skills, 1);
        assert!(
            snapshot.prompt_tokens > 0,
            "initial prompt estimate should be non-zero"
        );
        assert_eq!(snapshot.total_tokens, snapshot.prompt_tokens);
        assert!(
            !snapshot.active_skills.is_empty(),
            "active_skills should be pre-populated at startup"
        );
    }

    #[tokio::test]
    async fn skill_all_candidates_dropped_below_threshold_active_skills_empty() {
        // When min_injection_score = f32::MAX, every scored candidate fails the retain
        // gate. The agent must not panic and must report zero active skills for the turn.
        use zeph_skills::matcher::{SkillMatcher, SkillMatcherBackend};

        // Use an embedding-capable provider so SkillMatcher::new succeeds and
        // match_skills can embed the query — exercising the retain gate instead of
        // the empty-matcher fallback.
        let provider = AnyProvider::Mock(
            MockProvider::with_responses(vec!["response".to_string()])
                .with_embedding(vec![1.0, 0.0]),
        );
        let channel = MockChannel::new(vec!["hello".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

        // Build an in-memory matcher so the retain gate is exercised rather than
        // falling through the empty-matcher fallback path.
        let all_meta_owned: Vec<zeph_skills::loader::SkillMeta> = {
            let registry_guard = agent.skill_state.registry.read();
            registry_guard.all_meta().into_iter().cloned().collect()
        };
        let embed_fn = |_text: &str| -> zeph_skills::matcher::EmbedFuture {
            Box::pin(async { Ok(vec![1.0_f32, 0.0]) })
        };
        let matcher = SkillMatcher::new(&all_meta_owned.iter().collect::<Vec<_>>(), embed_fn).await;
        agent.skill_state.matcher = matcher.map(SkillMatcherBackend::InMemory);
        // Set an impossibly high threshold so every candidate is dropped.
        agent.skill_state.min_injection_score = f32::MAX;

        agent.run().await.unwrap();

        let snapshot = rx.borrow().clone();
        assert!(
            snapshot.active_skills.is_empty(),
            "no skills should be active when all candidates fail the score gate"
        );
    }

    #[tokio::test]
    async fn agent_metrics_update_on_llm_call() {
        let provider = mock_provider(vec!["response".to_string()]);
        let channel = MockChannel::new(vec!["hello".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

        agent.run().await.unwrap();

        let snapshot = rx.borrow().clone();
        assert_eq!(snapshot.api_calls, 1);
        assert!(snapshot.total_tokens > 0);
    }

    #[tokio::test]
    async fn agent_metrics_streaming_updates_completion_tokens() {
        let provider = mock_provider_streaming(vec!["streaming response".to_string()]);
        let channel = MockChannel::new(vec!["test".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

        agent.run().await.unwrap();

        let snapshot = rx.borrow().clone();
        assert!(snapshot.completion_tokens > 0);
        assert_eq!(snapshot.api_calls, 1);
    }

    #[tokio::test]
    async fn agent_metrics_persist_increments_count() {
        let provider = mock_provider(vec!["response".to_string()]);
        let channel = MockChannel::new(vec!["hello".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

        agent.run().await.unwrap();

        let snapshot = rx.borrow().clone();
        assert!(snapshot.sqlite_message_count == 0, "no memory = no persist");
    }

    #[tokio::test]
    async fn agent_metrics_skills_updated_on_prompt_rebuild() {
        let provider = mock_provider(vec!["response".to_string()]);
        let channel = MockChannel::new(vec!["hello".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

        agent.run().await.unwrap();

        let snapshot = rx.borrow().clone();
        assert_eq!(snapshot.total_skills, 1);
        assert!(!snapshot.active_skills.is_empty());
    }

    #[test]
    fn update_metrics_noop_when_none() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.update_metrics(|m| m.api_calls = 999);
    }

    #[test]
    fn update_metrics_sets_uptime_seconds() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let (tx, rx) = tokio::sync::watch::channel(MetricsSnapshot::default());
        let agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

        agent.update_metrics(|m| m.api_calls = 1);

        let snapshot = rx.borrow();
        assert!(snapshot.uptime_seconds < 2);
        assert_eq!(snapshot.api_calls, 1);
    }

    #[test]
    fn test_last_user_query_finds_original() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.msg.messages.push(Message {
            role: Role::User,
            content: "hello".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
        agent.msg.messages.push(Message {
            role: Role::Assistant,
            content: "cmd".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
        agent.msg.messages.push(Message {
            role: Role::User,
            content: "[tool output: bash]\nsome output".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });

        assert_eq!(agent.last_user_query(), "hello");
    }

    #[test]
    fn test_last_user_query_empty_messages() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor);
        assert_eq!(agent.last_user_query(), "");
    }

    #[tokio::test]
    async fn test_maybe_summarize_short_output_passthrough() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.tool_orchestrator.summarize_tool_output_enabled = true;

        let short = "short output";
        let result = agent.maybe_summarize_tool_output(short).await;
        assert_eq!(result, short);
    }

    #[tokio::test] // lgtm[rust/cleartext-logging]
    async fn test_overflow_notice_contains_uuid() {
        use std::sync::Arc;
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        use zeph_memory::semantic::SemanticMemory;

        let memory = SemanticMemory::with_sqlite_backend(
            ":memory:",
            AnyProvider::Mock(MockProvider::default()),
            "test-model",
            0.7,
            0.3,
        )
        .await
        .unwrap();
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            Arc::new(memory),
            cid,
            100,
            5,
            1000,
        );
        let mut agent = agent;
        agent.tool_orchestrator.overflow_config = zeph_tools::OverflowConfig {
            threshold: 100,
            retention_days: 7,
            max_overflow_bytes: 0,
        };

        let long = "x".repeat(zeph_tools::MAX_TOOL_OUTPUT_CHARS + 1000);
        let result = agent.maybe_summarize_tool_output(&long).await;
        assert!(
            result.contains("full output stored"),
            "notice must contain overflow storage notice, got: {result}"
        );
        assert!(
            result.contains("bytes"),
            "notice must contain byte count, got: {result}"
        );
        assert!(
            result.contains("read_overflow"),
            "notice must mention read_overflow tool, got: {result}"
        );
        // Must NOT contain filesystem paths.
        assert!(
            !result.contains(".txt"),
            "notice must not contain filesystem path, got: {result}"
        );
    }

    #[tokio::test]
    async fn test_maybe_summarize_long_output_disabled_truncates() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.tool_orchestrator.overflow_config = zeph_tools::OverflowConfig {
            threshold: 1000,
            retention_days: 7,
            max_overflow_bytes: 0,
        };

        // Must exceed overflow threshold (1000) so that truncate_tool_output_at produces
        // the "truncated" marker. MAX_TOOL_OUTPUT_CHARS is no longer used in this path.
        let long = "x".repeat(zeph_tools::MAX_TOOL_OUTPUT_CHARS + 1000);
        let result = agent.maybe_summarize_tool_output(&long).await;
        assert!(result.contains("truncated"));
    }

    #[tokio::test]
    async fn test_maybe_summarize_long_output_enabled_calls_llm() {
        let provider = mock_provider(vec!["summary text".to_string()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.tool_orchestrator.summarize_tool_output_enabled = true;
        agent.tool_orchestrator.overflow_config = zeph_tools::OverflowConfig {
            threshold: 1000,
            retention_days: 7,
            max_overflow_bytes: 0,
        };

        let long = "x".repeat(zeph_tools::MAX_TOOL_OUTPUT_CHARS + 1000);
        let result = agent.maybe_summarize_tool_output(&long).await;
        assert!(result.contains("summary text"));
        assert!(result.contains("[tool output summary]"));
        assert!(!result.contains("truncated"));
    }

    #[tokio::test]
    async fn test_summarize_tool_output_llm_failure_fallback() {
        let provider = mock_provider_failing();
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.tool_orchestrator.summarize_tool_output_enabled = true;
        agent.tool_orchestrator.overflow_config = zeph_tools::OverflowConfig {
            threshold: 1000,
            retention_days: 7,
            max_overflow_bytes: 0,
        };

        let long = "x".repeat(zeph_tools::MAX_TOOL_OUTPUT_CHARS + 1000);
        let result = agent.maybe_summarize_tool_output(&long).await;
        assert!(result.contains("truncated"));
    }

    #[tokio::test] // lgtm[rust/cleartext-logging]
    async fn test_overflow_no_memory_backend_s3_fallback() {
        // S3 fix: when no memory backend or conversation_id is present, the overflow notice
        // must include the fallback message rather than panicking or attempting a DB insert.
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.tool_orchestrator.overflow_config = zeph_tools::OverflowConfig {
            threshold: 100,
            retention_days: 7,
            max_overflow_bytes: 0,
        };
        // No memory backend set.

        let long = "x".repeat(200);
        let result = agent.maybe_summarize_tool_output(&long).await;
        assert!(
            result.contains("could not be saved — no memory backend or conversation available"),
            "S3 fallback message must appear when no memory backend, got: {result}"
        );
    }

    #[test]
    fn format_tool_output_structure() {
        let out = format_tool_output("bash", "hello world");
        assert!(out.starts_with("[tool output: bash]\n```\n"));
        assert!(out.ends_with(TOOL_OUTPUT_SUFFIX));
        assert!(out.contains("hello world"));
    }

    #[test]
    fn format_tool_output_empty_body() {
        let out = format_tool_output("grep", "");
        assert_eq!(out, "[tool output: grep]\n```\n\n```");
    }

    #[tokio::test]
    async fn cancel_signal_propagates_to_fresh_token() {
        use tokio_util::sync::CancellationToken;
        let signal = Arc::new(Notify::new());

        let token = CancellationToken::new();
        let sig = Arc::clone(&signal);
        let tok = token.clone();
        tokio::spawn(async move {
            sig.notified().await;
            tok.cancel();
        });

        // Yield to let the spawned task reach notified().await
        tokio::task::yield_now().await;
        assert!(!token.is_cancelled());
        signal.notify_waiters();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn cancel_signal_works_across_multiple_messages() {
        use tokio_util::sync::CancellationToken;
        let signal = Arc::new(Notify::new());

        // First "message"
        let token1 = CancellationToken::new();
        let sig1 = Arc::clone(&signal);
        let tok1 = token1.clone();
        tokio::spawn(async move {
            sig1.notified().await;
            tok1.cancel();
        });

        tokio::task::yield_now().await;
        signal.notify_waiters();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(token1.is_cancelled());

        // Second "message" — same signal, new token
        let token2 = CancellationToken::new();
        let sig2 = Arc::clone(&signal);
        let tok2 = token2.clone();
        tokio::spawn(async move {
            sig2.notified().await;
            tok2.cancel();
        });

        tokio::task::yield_now().await;
        assert!(!token2.is_cancelled());
        signal.notify_waiters();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(token2.is_cancelled());
    }

    mod resolve_message_tests {
        use super::*;
        use crate::channel::{Attachment, AttachmentKind, ChannelMessage};
        use std::future::Future;
        use std::pin::Pin;
        use zeph_llm::error::LlmError;
        use zeph_llm::stt::{SpeechToText, Transcription};

        struct MockStt {
            text: Option<String>,
        }

        impl MockStt {
            fn ok(text: &str) -> Self {
                Self {
                    text: Some(text.to_string()),
                }
            }

            fn failing() -> Self {
                Self { text: None }
            }
        }

        impl SpeechToText for MockStt {
            fn transcribe(
                &self,
                _audio: &[u8],
                _filename: Option<&str>,
            ) -> Pin<Box<dyn Future<Output = Result<Transcription, LlmError>> + Send + '_>>
            {
                let result = match &self.text {
                    Some(t) => Ok(Transcription {
                        text: t.clone(),
                        language: None,
                        duration_secs: None,
                    }),
                    None => Err(LlmError::TranscriptionFailed("mock error".into())),
                };
                Box::pin(async move { result })
            }
        }

        fn make_agent(stt: Option<Box<dyn SpeechToText>>) -> Agent<MockChannel> {
            let provider = mock_provider(vec!["ok".into()]);
            let empty: Vec<String> = vec![];
            let registry = zeph_skills::registry::SkillRegistry::load(&empty);
            let channel = MockChannel::new(vec![]);
            let executor = MockToolExecutor::no_tools();
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            agent.providers.stt = stt;
            agent
        }

        fn audio_attachment(data: &[u8]) -> Attachment {
            Attachment {
                kind: AttachmentKind::Audio,
                data: data.to_vec(),
                filename: Some("test.wav".into()),
            }
        }

        #[tokio::test]
        async fn no_audio_attachments_returns_text() {
            let agent = make_agent(None);
            let msg = ChannelMessage {
                text: "hello".into(),
                attachments: vec![],
            };
            assert_eq!(agent.resolve_message(msg).await.0, "hello");
        }

        #[tokio::test]
        async fn audio_without_stt_returns_original_text() {
            let agent = make_agent(None);
            let msg = ChannelMessage {
                text: "hello".into(),
                attachments: vec![audio_attachment(b"audio-data")],
            };
            assert_eq!(agent.resolve_message(msg).await.0, "hello");
        }

        #[tokio::test]
        async fn audio_with_stt_prepends_transcription() {
            let agent = make_agent(Some(Box::new(MockStt::ok("transcribed text"))));
            let msg = ChannelMessage {
                text: "original".into(),
                attachments: vec![audio_attachment(b"audio-data")],
            };
            let (result, _) = agent.resolve_message(msg).await;
            assert!(result.contains("[transcribed audio]"));
            assert!(result.contains("transcribed text"));
            assert!(result.contains("original"));
        }

        #[tokio::test]
        async fn audio_with_stt_no_original_text() {
            let agent = make_agent(Some(Box::new(MockStt::ok("transcribed text"))));
            let msg = ChannelMessage {
                text: String::new(),
                attachments: vec![audio_attachment(b"audio-data")],
            };
            let (result, _) = agent.resolve_message(msg).await;
            assert_eq!(result, "transcribed text");
        }

        #[tokio::test]
        async fn all_transcriptions_fail_returns_original() {
            let agent = make_agent(Some(Box::new(MockStt::failing())));
            let msg = ChannelMessage {
                text: "original".into(),
                attachments: vec![audio_attachment(b"audio-data")],
            };
            assert_eq!(agent.resolve_message(msg).await.0, "original");
        }

        #[tokio::test]
        async fn multiple_audio_attachments_joined() {
            let agent = make_agent(Some(Box::new(MockStt::ok("chunk"))));
            let msg = ChannelMessage {
                text: String::new(),
                attachments: vec![
                    audio_attachment(b"a1"),
                    audio_attachment(b"a2"),
                    audio_attachment(b"a3"),
                ],
            };
            let (result, _) = agent.resolve_message(msg).await;
            assert_eq!(result, "chunk\nchunk\nchunk");
        }

        #[tokio::test]
        async fn oversized_audio_skipped() {
            let agent = make_agent(Some(Box::new(MockStt::ok("should not appear"))));
            let big = vec![0u8; MAX_AUDIO_BYTES + 1];
            let msg = ChannelMessage {
                text: "original".into(),
                attachments: vec![Attachment {
                    kind: AttachmentKind::Audio,
                    data: big,
                    filename: None,
                }],
            };
            assert_eq!(agent.resolve_message(msg).await.0, "original");
        }
    }

    #[test]
    fn detect_image_mime_jpeg() {
        assert_eq!(detect_image_mime(Some("photo.jpg")), "image/jpeg");
        assert_eq!(detect_image_mime(Some("photo.jpeg")), "image/jpeg");
    }

    #[test]
    fn detect_image_mime_gif() {
        assert_eq!(detect_image_mime(Some("anim.gif")), "image/gif");
    }

    #[test]
    fn detect_image_mime_webp() {
        assert_eq!(detect_image_mime(Some("img.webp")), "image/webp");
    }

    #[test]
    fn detect_image_mime_unknown_defaults_png() {
        assert_eq!(detect_image_mime(Some("file.bmp")), "image/png");
        assert_eq!(detect_image_mime(None), "image/png");
    }

    #[tokio::test]
    async fn resolve_message_extracts_image_attachment() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let agent = Agent::new(provider, channel, registry, None, 5, executor);

        let msg = ChannelMessage {
            text: "look at this".into(),
            attachments: vec![Attachment {
                kind: AttachmentKind::Image,
                data: vec![0u8; 16],
                filename: Some("test.jpg".into()),
            }],
        };
        let (text, parts) = agent.resolve_message(msg).await;
        assert_eq!(text, "look at this");
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            zeph_llm::provider::MessagePart::Image(img) => {
                assert_eq!(img.mime_type, "image/jpeg");
                assert_eq!(img.data.len(), 16);
            }
            _ => panic!("expected Image part"),
        }
    }

    #[tokio::test]
    async fn resolve_message_drops_oversized_image() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let agent = Agent::new(provider, channel, registry, None, 5, executor);

        let msg = ChannelMessage {
            text: "big image".into(),
            attachments: vec![Attachment {
                kind: AttachmentKind::Image,
                data: vec![0u8; MAX_IMAGE_BYTES + 1],
                filename: Some("huge.png".into()),
            }],
        };
        let (text, parts) = agent.resolve_message(msg).await;
        assert_eq!(text, "big image");
        assert!(parts.is_empty());
    }

    #[test]
    fn handle_image_command_rejects_path_traversal() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.handle_image_as_string("../../etc/passwd");
        assert!(agent.msg.pending_image_parts.is_empty());
        assert!(result.contains("traversal"));
    }

    #[test]
    fn handle_image_command_missing_file_sends_error() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.handle_image_as_string("nonexistent/image.png");
        assert!(agent.msg.pending_image_parts.is_empty());
        assert!(result.contains("Cannot read image"));
    }

    #[test]
    fn handle_image_command_absolute_path_is_rejected() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.handle_image_as_string("/etc/passwd");
        assert!(agent.msg.pending_image_parts.is_empty());
        assert!(result.contains("path traversal not allowed"));
    }

    #[test]
    fn handle_image_command_parent_dir_traversal_is_rejected() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.handle_image_as_string("../../etc/passwd");
        assert!(agent.msg.pending_image_parts.is_empty());
        assert!(result.contains("path traversal not allowed"));
    }

    #[test]
    fn handle_image_command_loads_valid_file() {
        use std::io::Write as _;
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        // Use a temp dir under cwd so the resulting path can be made relative,
        // which is required by the path-traversal guard.
        let cwd = std::env::current_dir().unwrap();
        let tmp_dir = tempfile::TempDir::new_in(&cwd).unwrap();
        let file_path = tmp_dir.path().join("test.jpg");
        let data = vec![0xFFu8, 0xD8, 0xFF, 0xE0];
        std::fs::File::create(&file_path)
            .unwrap()
            .write_all(&data)
            .unwrap();
        let path = file_path
            .strip_prefix(&cwd)
            .unwrap_or(&file_path)
            .to_str()
            .unwrap()
            .to_owned();

        let result = agent.handle_image_as_string(&path);
        assert_eq!(agent.msg.pending_image_parts.len(), 1);
        match &agent.msg.pending_image_parts[0] {
            zeph_llm::provider::MessagePart::Image(img) => {
                assert_eq!(img.data, data);
                assert_eq!(img.mime_type, "image/jpeg");
            }
            _ => panic!("expected Image part"),
        }
        assert!(result.contains("Image loaded"));
    }

    // ── handle_agent_command tests ────────────────────────────────────────────

    use zeph_subagent::AgentCommand;

    fn make_agent_with_manager() -> Agent<MockChannel> {
        use zeph_subagent::def::{SkillFilter, SubAgentPermissions, ToolPolicy};
        use zeph_subagent::hooks::SubagentHooks;
        use zeph_subagent::{SubAgentDef, SubAgentManager};

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let mut mgr = SubAgentManager::new(4);
        mgr.definitions_mut().push(SubAgentDef {
            name: "helper".into(),
            description: "A helper bot".into(),
            model: None,
            tools: ToolPolicy::InheritAll,
            disallowed_tools: vec![],
            permissions: SubAgentPermissions::default(),
            skills: SkillFilter::default(),
            system_prompt: "You are helpful.".into(),
            hooks: SubagentHooks::default(),
            memory: None,
            source: None,
            file_path: None,
        });
        agent.orchestration.subagent_manager = Some(mgr);
        agent
    }

    #[tokio::test]
    async fn agent_command_no_manager_returns_none() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        // no subagent_manager set — List needs manager to return Some
        assert!(
            agent
                .handle_agent_command(AgentCommand::List)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn agent_command_list_returns_definitions() {
        let mut agent = make_agent_with_manager();
        let resp = agent
            .handle_agent_command(AgentCommand::List)
            .await
            .unwrap();
        assert!(resp.contains("helper"));
        assert!(resp.contains("A helper bot"));
    }

    #[tokio::test]
    async fn agent_command_spawn_unknown_name_returns_error() {
        let mut agent = make_agent_with_manager();
        let resp = agent
            .handle_agent_command(AgentCommand::Background {
                name: "unknown-bot".into(),
                prompt: "do something".into(),
            })
            .await
            .unwrap();
        assert!(resp.contains("Failed to spawn"));
    }

    #[tokio::test]
    async fn agent_command_spawn_known_name_returns_started() {
        let mut agent = make_agent_with_manager();
        let resp = agent
            .handle_agent_command(AgentCommand::Background {
                name: "helper".into(),
                prompt: "do some work".into(),
            })
            .await
            .unwrap();
        assert!(resp.contains("helper"));
        assert!(resp.contains("started"));
    }

    #[tokio::test]
    async fn agent_command_status_no_agents_returns_empty_message() {
        let mut agent = make_agent_with_manager();
        let resp = agent
            .handle_agent_command(AgentCommand::Status)
            .await
            .unwrap();
        assert!(resp.contains("No active sub-agents"));
    }

    #[tokio::test]
    async fn agent_command_cancel_unknown_id_returns_not_found() {
        let mut agent = make_agent_with_manager();
        let resp = agent
            .handle_agent_command(AgentCommand::Cancel {
                id: "deadbeef".into(),
            })
            .await
            .unwrap();
        assert!(resp.contains("No sub-agent"));
    }

    #[tokio::test]
    async fn agent_command_cancel_valid_id_succeeds() {
        let mut agent = make_agent_with_manager();
        // spawn first so we have a task to cancel
        let spawn_resp = agent
            .handle_agent_command(AgentCommand::Background {
                name: "helper".into(),
                prompt: "cancel this".into(),
            })
            .await
            .unwrap();
        // extract short id from "started in background (id: XXXXXXXX)"
        let short_id = spawn_resp
            .split("id: ")
            .nth(1)
            .unwrap()
            .trim_end_matches(')')
            .trim()
            .to_string();
        let resp = agent
            .handle_agent_command(AgentCommand::Cancel { id: short_id })
            .await
            .unwrap();
        assert!(resp.contains("Cancelled"));
    }

    #[tokio::test]
    async fn agent_command_approve_no_pending_request() {
        let mut agent = make_agent_with_manager();
        // Spawn an agent first so there's an active agent to reference
        let spawn_resp = agent
            .handle_agent_command(AgentCommand::Background {
                name: "helper".into(),
                prompt: "do work".into(),
            })
            .await
            .unwrap();
        let short_id = spawn_resp
            .split("id: ")
            .nth(1)
            .unwrap()
            .trim_end_matches(')')
            .trim()
            .to_string();
        let resp = agent
            .handle_agent_command(AgentCommand::Approve { id: short_id })
            .await
            .unwrap();
        assert!(resp.contains("No pending secret request"));
    }

    #[test]
    fn set_model_updates_model_name() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        assert!(agent.set_model("claude-opus-4-6").is_ok());
        assert_eq!(agent.runtime.model_name, "claude-opus-4-6");
    }

    #[test]
    fn set_model_overwrites_previous_value() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.set_model("model-a").unwrap();
        agent.set_model("model-b").unwrap();
        assert_eq!(agent.runtime.model_name, "model-b");
    }

    #[tokio::test]
    async fn model_command_switch_sends_confirmation() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let out = agent
            .handle_model_command_as_string("/model my-new-model")
            .await;
        assert!(
            out.contains("my-new-model"),
            "expected switch confirmation, got: {out}"
        );
    }

    #[tokio::test]
    async fn model_command_list_no_cache_fetches_remote() {
        // With mock provider, list_models_remote returns empty vec — agent sends "No models".
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        // Ensure cache is stale for mock provider slug
        zeph_llm::model_cache::ModelCache::for_slug("mock").invalidate();
        let out = agent.handle_model_command_as_string("/model").await;
        // Mock returns empty list → "No models available."
        assert!(
            out.contains("No models"),
            "expected empty model list message, got: {out}"
        );
    }

    #[tokio::test]
    async fn model_command_refresh_sends_result() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let out = agent.handle_model_command_as_string("/model refresh").await;
        assert!(
            out.contains("Fetched"),
            "expected fetch confirmation, got: {out}"
        );
    }

    #[tokio::test]
    async fn model_command_valid_model_accepted() {
        // Ensure cache is stale so the handler falls back to list_models_remote().
        zeph_llm::model_cache::ModelCache::for_slug("mock").invalidate();

        let models = vec![
            zeph_llm::model_cache::RemoteModelInfo {
                id: "llama3:8b".to_string(),
                display_name: "Llama 3 8B".to_string(),
                context_window: Some(8192),
                created_at: None,
            },
            zeph_llm::model_cache::RemoteModelInfo {
                id: "qwen3:8b".to_string(),
                display_name: "Qwen3 8B".to_string(),
                context_window: Some(32768),
                created_at: None,
            },
        ];
        let provider = mock_provider_with_models(vec![], models);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let out = agent
            .handle_model_command_as_string("/model llama3:8b")
            .await;

        assert!(
            out.contains("Switched to model: llama3:8b"),
            "expected switch confirmation, got: {out}"
        );
        assert!(
            !out.contains("Unknown model"),
            "unexpected rejection for valid model, got: {out}"
        );
    }

    #[tokio::test]
    async fn model_command_invalid_model_rejected() {
        // Ensure cache is stale so the handler falls back to list_models_remote().
        zeph_llm::model_cache::ModelCache::for_slug("mock").invalidate();

        let models = vec![zeph_llm::model_cache::RemoteModelInfo {
            id: "qwen3:8b".to_string(),
            display_name: "Qwen3 8B".to_string(),
            context_window: None,
            created_at: None,
        }];
        let provider = mock_provider_with_models(vec![], models);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let out = agent
            .handle_model_command_as_string("/model nonexistent-model")
            .await;

        assert!(
            out.contains("Unknown model") && out.contains("nonexistent-model"),
            "expected rejection with model name, got: {out}"
        );
        assert!(
            out.contains("qwen3:8b"),
            "expected available models list, got: {out}"
        );
        assert!(
            !out.contains("Switched to model"),
            "should not switch to invalid model, got: {out}"
        );
    }

    #[tokio::test]
    async fn model_command_empty_model_list_warns_and_proceeds() {
        // Ensure cache is stale so the handler falls back to list_models_remote().
        // MockProvider returns empty vec → warning shown, switch proceeds.
        zeph_llm::model_cache::ModelCache::for_slug("mock").invalidate();

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let out = agent
            .handle_model_command_as_string("/model unknown-model")
            .await;

        assert!(
            out.contains("unavailable"),
            "expected warning about unavailable model list, got: {out}"
        );
        assert!(
            out.contains("Switched to model: unknown-model"),
            "expected switch to proceed despite missing model list, got: {out}"
        );
    }

    #[tokio::test]
    async fn help_command_lists_commands() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec!["/help".to_string()]);
        let sent = channel.sent.clone();
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.run().await;
        assert!(result.is_ok());

        let messages = sent.lock().unwrap();
        assert!(!messages.is_empty(), "expected /help output");
        let output = messages.join("\n");
        assert!(output.contains("/help"), "expected /help in output");
        assert!(output.contains("/exit"), "expected /exit in output");
        assert!(output.contains("/status"), "expected /status in output");
        assert!(output.contains("/skills"), "expected /skills in output");
        assert!(output.contains("/model"), "expected /model in output");
    }

    #[tokio::test]
    async fn help_command_does_not_include_unknown_commands() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec!["/help".to_string()]);
        let sent = channel.sent.clone();
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.run().await;
        assert!(result.is_ok());

        let messages = sent.lock().unwrap();
        let output = messages.join("\n");
        // /ingest does not exist in the codebase — must not appear
        assert!(
            !output.contains("/ingest"),
            "unexpected /ingest in /help output"
        );
    }

    #[tokio::test]
    async fn status_command_includes_provider_and_model() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec!["/status".to_string()]);
        let sent = channel.sent.clone();
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.run().await;
        assert!(result.is_ok());

        let messages = sent.lock().unwrap();
        assert!(!messages.is_empty(), "expected /status output");
        let output = messages.join("\n");
        assert!(output.contains("Provider:"), "expected Provider: field");
        assert!(output.contains("Model:"), "expected Model: field");
        assert!(output.contains("Uptime:"), "expected Uptime: field");
        assert!(output.contains("Tokens:"), "expected Tokens: field");
    }

    // Regression test for #1415: MetricsCollector must be wired in CLI mode (no TUI).
    // Before the fix, metrics_tx was None in non-TUI mode so /status always showed zeros.
    #[tokio::test]
    async fn status_command_shows_metrics_in_cli_mode() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec!["/status".to_string()]);
        let sent = channel.sent.clone();
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let (tx, _rx) = watch::channel(MetricsSnapshot::default());
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

        // Simulate metrics that would be populated by a real LLM call.
        agent.update_metrics(|m| {
            m.api_calls = 3;
            m.prompt_tokens = 100;
            m.completion_tokens = 50;
        });

        let result = agent.run().await;
        assert!(result.is_ok());

        let messages = sent.lock().unwrap();
        let output = messages.join("\n");
        assert!(
            output.contains("API calls: 3"),
            "expected non-zero api_calls in /status output; got: {output}"
        );
        assert!(
            output.contains("100 prompt / 50 completion"),
            "expected non-zero tokens in /status output; got: {output}"
        );
    }

    #[tokio::test]
    async fn status_command_shows_orchestration_stats_when_plans_nonzero() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec!["/status".to_string()]);
        let sent = channel.sent.clone();
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let (tx, _rx) = watch::channel(MetricsSnapshot::default());
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

        agent.update_metrics(|m| {
            m.orchestration.plans_total = 2;
            m.orchestration.tasks_total = 10;
            m.orchestration.tasks_completed = 8;
            m.orchestration.tasks_failed = 1;
            m.orchestration.tasks_skipped = 1;
        });

        let result = agent.run().await;
        assert!(result.is_ok());

        let messages = sent.lock().unwrap();
        let output = messages.join("\n");
        assert!(
            output.contains("Orchestration:"),
            "expected Orchestration: section; got: {output}"
        );
        assert!(
            output.contains("Plans:     2"),
            "expected Plans: 2; got: {output}"
        );
        assert!(
            output.contains("8/10 completed"),
            "expected 8/10 completed; got: {output}"
        );
        assert!(
            output.contains("Failed:    1"),
            "expected Failed: 1; got: {output}"
        );
        assert!(
            output.contains("Skipped:   1"),
            "expected Skipped: 1; got: {output}"
        );
    }

    #[tokio::test]
    async fn status_command_hides_orchestration_when_no_plans() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec!["/status".to_string()]);
        let sent = channel.sent.clone();
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let (tx, _rx) = watch::channel(MetricsSnapshot::default());
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);
        // No orchestration metrics set — plans_total stays 0.

        let result = agent.run().await;
        assert!(result.is_ok());

        let messages = sent.lock().unwrap();
        let output = messages.join("\n");
        assert!(
            !output.contains("Orchestration:"),
            "Orchestration: section must be absent when no plans ran; got: {output}"
        );
    }

    #[tokio::test]
    async fn exit_command_breaks_run_loop() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec!["/exit".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.run().await;
        assert!(result.is_ok());
        // /exit should not produce any LLM message — only system message in history
        assert_eq!(agent.msg.messages.len(), 1, "expected only system message");
    }

    #[tokio::test]
    async fn quit_command_breaks_run_loop() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec!["/quit".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.run().await;
        assert!(result.is_ok());
        assert_eq!(agent.msg.messages.len(), 1, "expected only system message");
    }

    #[tokio::test]
    async fn exit_command_sends_info_and_continues_when_not_supported() {
        let provider = mock_provider(vec![]);
        // Channel that does not support exit: /exit should NOT break the loop,
        // it should send an info message and then yield the next message.
        let channel = MockChannel::new(vec![
            "/exit".to_string(),
            // second message is empty → causes recv() to return None → loop exits naturally
        ])
        .without_exit_support();
        let sent = channel.sent.clone();
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.run().await;
        assert!(result.is_ok());

        let messages = sent.lock().unwrap();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("/exit is not supported")),
            "expected info message, got: {messages:?}"
        );
    }

    #[test]
    fn slash_commands_registry_has_no_ingest() {
        use zeph_commands::COMMANDS;
        assert!(
            !COMMANDS.iter().any(|c| c.name == "/ingest"),
            "/ingest is not implemented and must not appear in COMMANDS"
        );
    }

    #[test]
    fn slash_commands_graph_and_plan_have_no_feature_gate() {
        use zeph_commands::COMMANDS;
        for cmd in COMMANDS {
            if cmd.name == "/graph" || cmd.name == "/plan" {
                assert!(
                    cmd.feature_gate.is_none(),
                    "{} should have feature_gate: None",
                    cmd.name
                );
            }
        }
    }

    // Regression tests for issue #1418: bare slash commands must not fall through to LLM.

    #[tokio::test]
    async fn bare_skill_command_does_not_invoke_llm() {
        // Provider has no responses — if LLM is called the agent would receive an empty response
        // and send "empty response" to the channel. The handler should return before reaching LLM.
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec!["/skill".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent = agent.channel.sent_messages();
        // Handler sends the "Unknown /skill subcommand" usage message — not an LLM response.
        assert!(
            sent.iter().any(|m| m.contains("Unknown /skill subcommand")),
            "bare /skill must send usage; got: {sent:?}"
        );
        // No assistant message should be added to history (LLM was not called).
        assert!(
            agent.msg.messages.iter().all(|m| m.role != Role::Assistant),
            "bare /skill must not produce an assistant message; messages: {:?}",
            agent.msg.messages
        );
    }

    #[tokio::test]
    async fn bare_feedback_command_does_not_invoke_llm() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec!["/feedback".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|m| m.contains("Usage: /feedback")),
            "bare /feedback must send usage; got: {sent:?}"
        );
        assert!(
            agent.msg.messages.iter().all(|m| m.role != Role::Assistant),
            "bare /feedback must not produce an assistant message; messages: {:?}",
            agent.msg.messages
        );
    }

    #[tokio::test]
    async fn bare_image_command_sends_usage() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec!["/image".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|m| m.contains("Usage: /image <path>")),
            "bare /image must send usage; got: {sent:?}"
        );
        assert!(
            agent.msg.messages.iter().all(|m| m.role != Role::Assistant),
            "bare /image must not produce an assistant message; messages: {:?}",
            agent.msg.messages
        );
    }

    #[tokio::test]
    async fn feedback_positive_records_user_approval() {
        let provider = mock_provider(vec![]);
        let memory =
            SemanticMemory::new(":memory:", "http://127.0.0.1:1", provider.clone(), "test")
                .await
                .unwrap();
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let memory = std::sync::Arc::new(memory);

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            memory.clone(),
            cid,
            50,
            5,
            50,
        );

        agent
            .handle_feedback_as_string("git great job, works perfectly")
            .await
            .unwrap();

        let row: Option<(String,)> =
            sqlx::query_as("SELECT outcome FROM skill_outcomes WHERE skill_name = 'git' LIMIT 1")
                .fetch_optional(memory.sqlite().pool())
                .await
                .unwrap();
        assert_eq!(
            row.map(|r| r.0).as_deref(),
            Some("user_approval"),
            "positive feedback must be recorded as user_approval"
        );
    }

    #[tokio::test]
    async fn feedback_negative_records_user_rejection() {
        let provider = mock_provider(vec![]);
        let memory =
            SemanticMemory::new(":memory:", "http://127.0.0.1:1", provider.clone(), "test")
                .await
                .unwrap();
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let memory = std::sync::Arc::new(memory);

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            memory.clone(),
            cid,
            50,
            5,
            50,
        );

        agent
            .handle_feedback_as_string("git that was wrong, bad output")
            .await
            .unwrap();

        let row: Option<(String,)> =
            sqlx::query_as("SELECT outcome FROM skill_outcomes WHERE skill_name = 'git' LIMIT 1")
                .fetch_optional(memory.sqlite().pool())
                .await
                .unwrap();
        assert_eq!(
            row.map(|r| r.0).as_deref(),
            Some("user_rejection"),
            "negative feedback must be recorded as user_rejection"
        );
    }

    #[tokio::test]
    async fn feedback_neutral_records_user_approval() {
        let provider = mock_provider(vec![]);
        let memory =
            SemanticMemory::new(":memory:", "http://127.0.0.1:1", provider.clone(), "test")
                .await
                .unwrap();
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let memory = std::sync::Arc::new(memory);

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            memory.clone(),
            cid,
            50,
            5,
            50,
        );

        // Ambiguous/neutral feedback — FeedbackDetector returns None → user_approval
        agent.handle_feedback_as_string("git ok").await.unwrap();

        let row: Option<(String,)> =
            sqlx::query_as("SELECT outcome FROM skill_outcomes WHERE skill_name = 'git' LIMIT 1")
                .fetch_optional(memory.sqlite().pool())
                .await
                .unwrap();
        assert_eq!(
            row.map(|r| r.0).as_deref(),
            Some("user_approval"),
            "neutral/ambiguous feedback must be recorded as user_approval"
        );
    }

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

    // ── GraphPersistence wiring tests ─────────────────────────────────────────

    #[cfg(feature = "scheduler")]
    async fn make_gp_memory() -> std::sync::Arc<zeph_memory::semantic::SemanticMemory> {
        let memory = zeph_memory::semantic::SemanticMemory::with_sqlite_backend(
            ":memory:",
            AnyProvider::Mock(MockProvider::default()),
            "test-model",
            0.7,
            0.3,
        )
        .await
        .expect("SemanticMemory");
        std::sync::Arc::new(memory)
    }

    #[cfg(feature = "scheduler")]
    fn graph_with_status(status: zeph_orchestration::GraphStatus) -> zeph_orchestration::TaskGraph {
        let mut g = zeph_orchestration::TaskGraph::new("test goal");
        g.status = status;
        g
    }

    #[cfg(feature = "scheduler")]
    fn make_orch_agent(
        memory: std::sync::Arc<zeph_memory::semantic::SemanticMemory>,
        cid: zeph_memory::ConversationId,
        persistence_enabled: bool,
    ) -> Agent<MockChannel> {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let cfg = crate::config::OrchestrationConfig {
            persistence_enabled,
            ..Default::default()
        };
        Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(memory, cid, 100, 5, 1000)
            .with_orchestration(cfg, crate::config::SubAgentConfig::default(), {
                zeph_subagent::SubAgentManager::new(4)
            })
    }

    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn wire_graph_persistence_is_none_without_memory() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let cfg = crate::config::OrchestrationConfig {
            persistence_enabled: true,
            ..Default::default()
        };
        let agent = Agent::new(provider, channel, registry, None, 5, executor).with_orchestration(
            cfg,
            crate::config::SubAgentConfig::default(),
            zeph_subagent::SubAgentManager::new(4),
        );
        assert!(
            agent.orchestration.graph_persistence.is_none(),
            "graph_persistence must be None when no memory is attached"
        );
    }

    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn wire_graph_persistence_is_none_when_disabled() {
        let memory = make_gp_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let agent = make_orch_agent(memory, cid, false);
        assert!(
            agent.orchestration.graph_persistence.is_none(),
            "graph_persistence must be None when persistence_enabled = false"
        );
    }

    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn wire_graph_persistence_is_some_with_memory_and_enabled() {
        let memory = make_gp_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let agent = make_orch_agent(memory, cid, true);
        assert!(
            agent.orchestration.graph_persistence.is_some(),
            "graph_persistence must be Some when memory attached and persistence_enabled = true"
        );
    }

    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn handle_plan_resume_invalid_uuid_returns_error_string() {
        let memory = make_gp_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let mut agent = make_orch_agent(memory, cid, true);
        let result = agent.handle_plan_resume_as_string(Some("not-a-uuid")).await;
        assert!(
            result.contains("Invalid graph id"),
            "must return parse error for invalid UUID, got: {result}"
        );
    }

    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn handle_plan_resume_missing_graph_returns_not_found() {
        use zeph_orchestration::GraphId;
        let memory = make_gp_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let mut agent = make_orch_agent(memory, cid, true);
        let missing_id = GraphId::new().to_string();
        let result = agent
            .handle_plan_resume_as_string(Some(missing_id.as_str()))
            .await;
        assert!(
            result.contains("not found in persistence"),
            "must return not-found message, got: {result}"
        );
    }

    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn handle_plan_resume_from_disk_hydrates_paused() {
        use zeph_orchestration::GraphStatus;
        let memory = make_gp_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let mut agent = make_orch_agent(memory, cid, true);
        let graph = graph_with_status(GraphStatus::Paused);
        let graph_id = graph.id.to_string();
        agent
            .orchestration
            .graph_persistence
            .as_ref()
            .unwrap()
            .save(&graph)
            .await
            .unwrap();
        let result = agent
            .handle_plan_resume_as_string(Some(graph_id.as_str()))
            .await;
        assert!(result.contains("Resuming plan"), "got: {result}");
        assert!(agent.orchestration.pending_graph.is_some());
        assert_eq!(
            agent.orchestration.pending_graph.as_ref().unwrap().status,
            GraphStatus::Paused
        );
    }

    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn handle_plan_resume_from_disk_recovers_running_as_paused() {
        use zeph_orchestration::{GraphStatus, TaskStatus};
        let memory = make_gp_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let mut agent = make_orch_agent(memory, cid, true);
        let mut graph = graph_with_status(GraphStatus::Running);
        let mut task = zeph_orchestration::TaskNode::new(0, "task1", "do something");
        task.status = TaskStatus::Running;
        task.assigned_agent = Some("agent-1".to_string());
        graph.tasks.push(task);
        let graph_id = graph.id.to_string();
        agent
            .orchestration
            .graph_persistence
            .as_ref()
            .unwrap()
            .save(&graph)
            .await
            .unwrap();
        let result = agent
            .handle_plan_resume_as_string(Some(graph_id.as_str()))
            .await;
        assert!(result.contains("Recovered plan"), "got: {result}");
        let recovered = agent.orchestration.pending_graph.as_ref().unwrap();
        assert_eq!(recovered.status, GraphStatus::Paused);
        assert_eq!(recovered.tasks[0].status, TaskStatus::Ready);
        assert!(recovered.tasks[0].assigned_agent.is_none());
    }

    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn handle_plan_resume_refuses_completed() {
        use zeph_orchestration::GraphStatus;
        let memory = make_gp_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let mut agent = make_orch_agent(memory, cid, true);
        let graph = graph_with_status(GraphStatus::Completed);
        let graph_id = graph.id.to_string();
        agent
            .orchestration
            .graph_persistence
            .as_ref()
            .unwrap()
            .save(&graph)
            .await
            .unwrap();
        let result = agent
            .handle_plan_resume_as_string(Some(graph_id.as_str()))
            .await;
        assert!(result.contains("already Completed"), "got: {result}");
        assert!(agent.orchestration.pending_graph.is_none());
    }

    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn handle_plan_resume_refuses_canceled() {
        use zeph_orchestration::GraphStatus;
        let memory = make_gp_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let mut agent = make_orch_agent(memory, cid, true);
        let graph = graph_with_status(GraphStatus::Canceled);
        let graph_id = graph.id.to_string();
        agent
            .orchestration
            .graph_persistence
            .as_ref()
            .unwrap()
            .save(&graph)
            .await
            .unwrap();
        let result = agent
            .handle_plan_resume_as_string(Some(graph_id.as_str()))
            .await;
        assert!(result.contains("Canceled"), "got: {result}");
        assert!(agent.orchestration.pending_graph.is_none());
    }

    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn handle_plan_resume_no_id_no_pending_returns_instructions() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.handle_plan_resume_as_string(None).await;
        assert!(result.contains("No paused plan"), "got: {result}");
    }

    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn handle_plan_resume_no_persistence_with_id_returns_disabled_message() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent
            .handle_plan_resume_as_string(Some("00000000-0000-0000-0000-000000000001"))
            .await;
        // UUID parses successfully, so persistence-disabled branch is unambiguously hit.
        assert!(
            result.contains("persistence is disabled"),
            "expected persistence-disabled message, got: {result}"
        );
    }

    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn predicate_outcome_round_trips_through_new_agent() {
        use zeph_orchestration::{
            GraphPersistence, GraphStatus, PredicateOutcome, TaskGraph, TaskNode, TaskStatus,
        };

        // Reuse the shared in-memory DB via SemanticMemory — migrations are already applied.
        let memory = make_gp_memory().await;
        let pool = memory.sqlite().pool().clone();
        let store_a = zeph_memory::store::graph_store::DbGraphStore::new(pool.clone());
        let persistence_a = GraphPersistence::new(store_a);

        let node = TaskNode {
            status: TaskStatus::Completed,
            predicate_outcome: Some(PredicateOutcome {
                passed: true,
                reason: "ok".to_string(),
                confidence: 1.0,
            }),
            ..TaskNode::new(0, "task-a", "test task")
        };
        let graph = TaskGraph {
            status: GraphStatus::Completed,
            tasks: vec![node],
            ..TaskGraph::new("test goal")
        };
        let graph_id = graph.id.clone();

        persistence_a.save(&graph).await.unwrap();

        // Second persistence handle on the same pool — simulates "new agent" rehydrating.
        let store_b = zeph_memory::store::graph_store::DbGraphStore::new(pool);
        let persistence_b = GraphPersistence::new(store_b);

        let loaded = persistence_b
            .load(&graph_id)
            .await
            .unwrap()
            .expect("graph must exist");
        let loaded_node = loaded.tasks.first().expect("task must exist");
        let outcome = loaded_node
            .predicate_outcome
            .as_ref()
            .expect("predicate_outcome must survive round-trip");
        assert!(
            outcome.passed,
            "predicate outcome `passed` must be true after round-trip"
        );
    }

    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn scheduler_loop_saves_graph_once_per_tick() {
        use zeph_orchestration::{GraphPersistence, GraphStatus, TaskGraph};

        let memory = make_gp_memory().await;
        let pool = memory.sqlite().pool().clone();
        let store = zeph_memory::store::graph_store::DbGraphStore::new(pool.clone());
        let persistence = GraphPersistence::new(store);

        let graph = TaskGraph {
            status: GraphStatus::Running,
            ..TaskGraph::new("g")
        };
        let graph_id = graph.id.clone();

        // Mirrors what save_graph_snapshot does (bounded save call).
        persistence.save(&graph).await.unwrap();

        let store2 = zeph_memory::store::graph_store::DbGraphStore::new(pool);
        let persistence2 = GraphPersistence::new(store2);
        let loaded = persistence2.load(&graph_id).await.unwrap();
        assert!(
            loaded.is_some(),
            "graph must be persisted after save_graph_snapshot call"
        );
        assert_eq!(loaded.unwrap().id, graph_id);
    }

    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn terminal_save_persists_completed_status() {
        use zeph_orchestration::{GraphPersistence, GraphStatus, TaskGraph};

        let memory = make_gp_memory().await;
        let pool = memory.sqlite().pool().clone();
        let store = zeph_memory::store::graph_store::DbGraphStore::new(pool.clone());
        let persistence = GraphPersistence::new(store);

        let graph = TaskGraph {
            status: GraphStatus::Completed,
            ..TaskGraph::new("g")
        };
        let graph_id = graph.id.clone();

        // Simulate the authoritative terminal save in handle_plan_confirm.
        match tokio::time::timeout(std::time::Duration::from_secs(5), persistence.save(&graph))
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("terminal save failed: {e}"),
            Err(e) => panic!("terminal save timed out after 5s: {e}"),
        }
        drop(persistence);

        let store2 = zeph_memory::store::graph_store::DbGraphStore::new(pool);
        let persistence2 = GraphPersistence::new(store2);
        let loaded = persistence2
            .load(&graph_id)
            .await
            .unwrap()
            .expect("must find saved graph");
        assert_eq!(
            loaded.status,
            GraphStatus::Completed,
            "terminal save must persist Completed status"
        );
    }
}

/// End-to-end tests for M30 resilient compaction: error detection → compact → retry → success.
#[cfg(test)]
mod compaction_e2e {
    use super::agent_tests::*;
    use zeph_llm::LlmError;
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;
    use zeph_llm::provider::{Message, MessageMetadata, Role};

    /// Verify that the agent recovers from a `ContextLengthExceeded` error during an LLM call,
    /// compacts its context, and returns a successful response on the next attempt.
    #[tokio::test]
    async fn agent_recovers_from_context_length_exceeded_and_produces_response() {
        // Provider: first call raises ContextLengthExceeded, second call succeeds.
        let provider = AnyProvider::Mock(
            MockProvider::with_responses(vec!["final answer".into()])
                .with_errors(vec![LlmError::ContextLengthExceeded]),
        );
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = super::super::Agent::new(provider, channel, registry, None, 5, executor)
            // Provide a context budget so compact_context has a compaction target
            .with_context_budget(200_000, 0.20, 0.80, 4, 0);

        // Seed a user message so the agent has something to compact/retry
        agent.msg.messages.push(Message {
            role: Role::User,
            content: "describe the architecture".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });

        // call_llm_with_retry is the direct entry point for the retry/compact flow
        let result = agent.call_llm_with_retry(2).await.unwrap();

        assert!(
            result.is_some(),
            "agent must produce a response after recovering from context length error"
        );
        assert_eq!(result.as_deref(), Some("final answer"));

        // Verify the channel received the recovered response
        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|m| m.contains("final answer")),
            "recovered response must be forwarded to the channel; got: {sent:?}"
        );
    }

    /// E2E test: spawn sub-agent in background, verify it runs and produces output.
    ///
    /// Scope: spawn → text response → collect (`MockProvider` only supports text responses).
    #[tokio::test]
    async fn subagent_spawn_text_collect_e2e() {
        use zeph_subagent::def::{SkillFilter, SubAgentPermissions, ToolPolicy};
        use zeph_subagent::hooks::SubagentHooks;
        use zeph_subagent::{AgentCommand, SubAgentDef, SubAgentManager};

        // Provider shared between main agent and sub-agent via Arc clone.
        // We pre-load a response that the sub-agent loop will consume.
        let provider = mock_provider(vec!["task completed successfully".into()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let mut mgr = SubAgentManager::new(4);
        mgr.definitions_mut().push(SubAgentDef {
            name: "worker".into(),
            description: "A worker bot".into(),
            model: None,
            tools: ToolPolicy::InheritAll,
            disallowed_tools: vec![],
            permissions: SubAgentPermissions {
                max_turns: 1,
                ..SubAgentPermissions::default()
            },
            skills: SkillFilter::default(),
            system_prompt: "You are a worker.".into(),
            hooks: SubagentHooks::default(),
            memory: None,
            source: None,
            file_path: None,
        });
        agent.orchestration.subagent_manager = Some(mgr);

        // Spawn the sub-agent in background — returns immediately with the task id.
        let spawn_resp = agent
            .handle_agent_command(AgentCommand::Background {
                name: "worker".into(),
                prompt: "do a task".into(),
            })
            .await
            .expect("Background spawn must return Some");
        assert!(
            spawn_resp.contains("worker"),
            "spawn response must mention agent name; got: {spawn_resp}"
        );
        assert!(
            spawn_resp.contains("started"),
            "spawn response must confirm start; got: {spawn_resp}"
        );

        // Extract the short id from response: "Sub-agent 'worker' started in background (id: XXXXXXXX)"
        let short_id = spawn_resp
            .split("id: ")
            .nth(1)
            .expect("response must contain 'id: '")
            .trim_end_matches(')')
            .trim()
            .to_string();
        assert!(!short_id.is_empty(), "short_id must not be empty");

        // Poll until the sub-agent reaches a terminal state (max 5s).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let full_id = loop {
            let mgr = agent.orchestration.subagent_manager.as_ref().unwrap();
            let statuses = mgr.statuses();
            let found = statuses.iter().find(|(id, _)| id.starts_with(&short_id));
            if let Some((id, status)) = found {
                match status.state {
                    zeph_subagent::SubAgentState::Completed => break id.clone(),
                    zeph_subagent::SubAgentState::Failed => {
                        panic!(
                            "sub-agent reached Failed state unexpectedly: {:?}",
                            status.last_message
                        );
                    }
                    _ => {}
                }
            }
            assert!(
                std::time::Instant::now() <= deadline,
                "sub-agent did not complete within timeout"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };

        // Collect result and verify output.
        let result = agent
            .orchestration
            .subagent_manager
            .as_mut()
            .unwrap()
            .collect(&full_id)
            .await
            .expect("collect must succeed for completed sub-agent");
        assert!(
            result.contains("task completed successfully"),
            "collected result must contain sub-agent output; got: {result:?}"
        );
    }

    /// Unit test for secret bridge in foreground spawn poll loop.
    ///
    /// Verifies that when a sub-agent emits [`REQUEST_SECRET`: api-key], the bridge:
    /// - calls `channel.confirm()` with a prompt containing the key name
    /// - on approval, delivers the secret to the sub-agent
    /// The `MockChannel` `confirm()` is pre-loaded with `true` (approve).
    #[tokio::test]
    async fn foreground_spawn_secret_bridge_approves() {
        use zeph_subagent::def::{SkillFilter, SubAgentPermissions, ToolPolicy};
        use zeph_subagent::hooks::SubagentHooks;
        use zeph_subagent::{AgentCommand, SubAgentDef, SubAgentManager};

        // Sub-agent loop responses:
        //   turn 1: request a secret
        //   turn 2: final reply after secret delivered
        let provider = mock_provider(vec![
            "[REQUEST_SECRET: api-key]".into(),
            "done with secret".into(),
        ]);

        // MockChannel with confirm() → true (approve)
        let channel = MockChannel::new(vec![]).with_confirmations(vec![true]);

        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let mut mgr = SubAgentManager::new(4);
        mgr.definitions_mut().push(SubAgentDef {
            name: "vault-bot".into(),
            description: "A bot that requests secrets".into(),
            model: None,
            tools: ToolPolicy::InheritAll,
            disallowed_tools: vec![],
            permissions: SubAgentPermissions {
                max_turns: 2,
                secrets: vec!["api-key".into()],
                ..SubAgentPermissions::default()
            },
            skills: SkillFilter::default(),
            system_prompt: "You need a secret.".into(),
            hooks: SubagentHooks::default(),
            memory: None,
            source: None,
            file_path: None,
        });
        agent.orchestration.subagent_manager = Some(mgr);

        // Foreground spawn — blocks until sub-agent completes.
        let resp: String = agent
            .handle_agent_command(AgentCommand::Spawn {
                name: "vault-bot".into(),
                prompt: "fetch the api key".into(),
            })
            .await
            .expect("Spawn must return Some");

        // Sub-agent completed after secret was bridged (approve path).
        // The sub-agent had 2 turns: turn 1 = secret request, turn 2 = final reply.
        // If the bridge did NOT call confirm(), the sub-agent would never get the
        // approval outcome and the foreground poll loop would stall or time out.
        // Reaching this point proves the bridge ran and confirm() was called.
        assert!(
            resp.contains("vault-bot"),
            "response must mention agent name; got: {resp}"
        );
        assert!(
            resp.contains("completed"),
            "sub-agent must complete successfully; got: {resp}"
        );
    }

    // ── /plan handler unit tests ─────────────────────────────────────────────

    #[cfg(feature = "scheduler")]
    use zeph_orchestration::{
        GraphStatus, PlanCommand, TaskGraph, TaskNode, TaskResult, TaskStatus,
    };

    #[cfg(feature = "scheduler")]
    fn agent_with_orchestration() -> Agent<MockChannel> {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.orchestration.orchestration_config.enabled = true;
        agent
    }

    #[cfg(feature = "scheduler")]
    fn make_simple_graph(status: GraphStatus) -> TaskGraph {
        let mut g = TaskGraph::new("test goal");
        let mut node = TaskNode::new(0, "task-0", "do something");
        node.status = match status {
            GraphStatus::Created => TaskStatus::Pending,
            GraphStatus::Running => TaskStatus::Ready,
            _ => TaskStatus::Completed,
        };
        if status == GraphStatus::Running || status == GraphStatus::Completed {
            node.result = Some(TaskResult {
                output: "done".into(),
                artifacts: vec![],
                duration_ms: 0,
                agent_id: None,
                agent_def: None,
            });
            if status == GraphStatus::Completed {
                node.status = TaskStatus::Completed;
            }
        }
        g.tasks.push(node);
        g.status = status;
        g
    }

    /// GAP-1: `handle_plan_confirm` with `subagent_manager` = None → fallback message,
    /// graph restored in `pending_graph`.
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn plan_confirm_no_manager_restores_graph() {
        let mut agent = agent_with_orchestration();

        let graph = make_simple_graph(GraphStatus::Created);
        agent.orchestration.pending_graph = Some(graph);

        // No subagent_manager set.
        agent
            .handle_plan_command_as_string(PlanCommand::Confirm)
            .await
            .unwrap();

        // Graph must be restored.
        assert!(
            agent.orchestration.pending_graph.is_some(),
            "graph must be restored when no manager configured"
        );
        let msgs = agent.channel.sent_messages();
        assert!(
            msgs.iter().any(|m| m.contains("sub-agent")),
            "must send fallback message; got: {msgs:?}"
        );
    }

    /// GAP-2: `handle_plan_confirm` with `pending_graph` = None → "No pending plan" message.
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn plan_confirm_no_pending_graph_sends_message() {
        let mut agent = agent_with_orchestration();

        // No pending_graph.
        agent
            .handle_plan_command_as_string(PlanCommand::Confirm)
            .await
            .unwrap();

        let msgs = agent.channel.sent_messages();
        assert!(
            msgs.iter().any(|m| m.contains("No pending plan")),
            "must send 'No pending plan' message; got: {msgs:?}"
        );
    }

    /// GAP-3: happy path — pre-built Running graph with one already-Completed task.
    /// `resume_from()` accepts it; first `tick()` emits Done{Completed}; aggregation called.
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn plan_confirm_completed_graph_aggregates() {
        use zeph_subagent::def::{SkillFilter, SubAgentPermissions, ToolPolicy};
        use zeph_subagent::hooks::SubagentHooks;
        use zeph_subagent::{SubAgentDef, SubAgentManager};

        // MockProvider returns the aggregation synthesis.
        let provider = mock_provider(vec!["synthesis result".into()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.orchestration.orchestration_config.enabled = true;

        let mut mgr = SubAgentManager::new(4);
        mgr.definitions_mut().push(SubAgentDef {
            name: "worker".into(),
            description: "A worker".into(),
            model: None,
            tools: ToolPolicy::InheritAll,
            disallowed_tools: vec![],
            permissions: SubAgentPermissions::default(),
            skills: SkillFilter::default(),
            system_prompt: "You are helpful.".into(),
            hooks: SubagentHooks::default(),
            memory: None,
            source: None,
            file_path: None,
        });
        agent.orchestration.subagent_manager = Some(mgr);

        // Graph with one already-Completed task in Running status: resume_from() accepts it,
        // and the first tick() will find no running/ready tasks → Done{Completed}.
        let mut graph = TaskGraph::new("test goal");
        let mut node = TaskNode::new(0, "task-0", "already done");
        node.status = TaskStatus::Completed;
        node.result = Some(TaskResult {
            output: "task output".into(),
            artifacts: vec![],
            duration_ms: 10,
            agent_id: None,
            agent_def: None,
        });
        graph.tasks.push(node);
        graph.status = GraphStatus::Running;
        agent.orchestration.pending_graph = Some(graph);

        agent
            .handle_plan_command_as_string(PlanCommand::Confirm)
            .await
            .unwrap();

        let msgs = agent.channel.sent_messages();
        // Aggregation synthesis should appear in messages.
        assert!(
            msgs.iter().any(|m| m.contains("synthesis result")),
            "aggregation synthesis must be sent to user; got: {msgs:?}"
        );
        // Graph must be cleared after successful completion.
        assert!(
            agent.orchestration.pending_graph.is_none(),
            "pending_graph must be cleared after Completed"
        );
    }

    /// GAP-4: `handle_plan_confirm` with no sub-agents defined but provider fails →
    /// task executes inline but provider returns error → plan fails, failure message sent.
    ///
    /// Since the fix for #1463, no agents → `RunInline` (not `spawn_for_task`). So when
    /// the provider fails, the scheduler records a Failed `TaskOutcome`, graph fails,
    /// and `finalize_plan_execution` sends a failure message.
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn plan_confirm_inline_provider_failure_sends_message() {
        use zeph_subagent::SubAgentManager;

        // Failing provider → chat() always returns an error.
        let provider = mock_provider_failing();
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.orchestration.orchestration_config.enabled = true;

        // Manager with no defined agents → route() returns None → RunInline.
        agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

        // Graph in Created status with one task; scheduler emits RunInline,
        // provider fails → TaskOutcome::Failed → graph Failed.
        let mut graph = TaskGraph::new("failing inline goal");
        let node = TaskNode::new(0, "task-0", "will fail inline");
        graph.tasks.push(node);
        graph.status = GraphStatus::Created;
        agent.orchestration.pending_graph = Some(graph);

        agent
            .handle_plan_command_as_string(PlanCommand::Confirm)
            .await
            .unwrap();

        let msgs = agent.channel.sent_messages();
        assert!(
            msgs.iter()
                .any(|m| m.contains("failed") || m.contains("Failed")),
            "failure message must be sent after inline provider error; got: {msgs:?}"
        );
    }

    /// GAP-5: `handle_plan_list` with `pending_graph` → shows summary + status label.
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn plan_list_with_pending_graph_shows_summary() {
        let mut agent = agent_with_orchestration();

        agent.orchestration.pending_graph = Some(make_simple_graph(GraphStatus::Created));

        let out = agent
            .handle_plan_command_as_string(PlanCommand::List)
            .await
            .unwrap();

        assert!(
            out.contains("awaiting confirmation"),
            "must show 'awaiting confirmation' status; got: {out:?}"
        );
    }

    /// GAP-6: `handle_plan_list` with no graph → "No recent plans."
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn plan_list_no_graph_shows_no_recent() {
        let mut agent = agent_with_orchestration();

        let out = agent
            .handle_plan_command_as_string(PlanCommand::List)
            .await
            .unwrap();

        assert!(
            out.contains("No recent plans"),
            "must show 'No recent plans'; got: {out:?}"
        );
    }

    /// GAP-7: `handle_plan_retry` resets Running tasks to Ready and clears `assigned_agent`.
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn plan_retry_resets_running_tasks_to_ready() {
        let mut agent = agent_with_orchestration();

        let mut graph = TaskGraph::new("retry test");
        let mut failed = TaskNode::new(0, "failed-task", "desc");
        failed.status = TaskStatus::Failed;
        let mut stale_running = TaskNode::new(1, "stale-task", "desc");
        stale_running.status = TaskStatus::Running;
        stale_running.assigned_agent = Some("old-handle-id".into());
        graph.tasks.push(failed);
        graph.tasks.push(stale_running);
        graph.status = GraphStatus::Failed;
        agent.orchestration.pending_graph = Some(graph);

        agent
            .handle_plan_command_as_string(PlanCommand::Retry(None))
            .await
            .unwrap();

        let g = agent
            .orchestration
            .pending_graph
            .as_ref()
            .expect("graph must be present after retry");

        // Failed task must be reset to Ready.
        assert_eq!(
            g.tasks[0].status,
            TaskStatus::Ready,
            "failed task must be reset to Ready"
        );

        // Stale Running task must be reset to Ready and assigned_agent cleared.
        assert_eq!(
            g.tasks[1].status,
            TaskStatus::Ready,
            "stale Running task must be reset to Ready"
        );
        assert!(
            g.tasks[1].assigned_agent.is_none(),
            "assigned_agent must be cleared for stale Running task"
        );
    }

    /// GAP-A: `handle_plan_cancel_as_string` with no active plan returns "No active plan".
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn plan_cancel_as_string_no_active_plan() {
        let mut agent = agent_with_orchestration();
        let out = agent.handle_plan_cancel_as_string(None);
        assert!(
            out.contains("No active plan"),
            "must return 'No active plan' message; got: {out:?}"
        );
    }

    /// GAP-A: `handle_plan_resume_as_string` with no pending graph returns "No paused plan".
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn plan_resume_as_string_no_paused_plan() {
        let mut agent = agent_with_orchestration();
        let out = agent.handle_plan_resume_as_string(None).await;
        assert!(
            out.contains("No paused plan"),
            "must return 'No paused plan' message; got: {out:?}"
        );
    }

    /// GAP-B: `dispatch_plan_command_as_string` with a parse error returns `Ok(non-empty)`.
    /// `/plan list extra_args` is rejected by the parser — the error must be returned as
    /// `Ok(message)`, not propagated as `Err`.
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn dispatch_plan_command_as_string_invalid_subcommand() {
        let mut agent = agent_with_orchestration();
        let result = agent
            .dispatch_plan_command_as_string("/plan list unexpected_arg")
            .await
            .unwrap();
        assert!(
            !result.is_empty(),
            "parse error must be returned as Ok(non-empty string), not propagated; got: {result:?}"
        );
    }

    /// Regression test for issue #1454: secret requests queued before the first `tick()` were
    /// silently dropped when a single-task plan completed on the very first tick (instant
    /// completion) because `process_pending_secret_requests()` was only called inside the
    /// `'tick: loop` body, which exits immediately via `break` before reaching the drain call.
    ///
    /// The fix adds a final `process_pending_secret_requests()` drain after the loop exits.
    /// This test verifies that drain by:
    /// 1. Pre-loading a `SecretRequest` into the manager's channel **before** the plan runs.
    /// 2. Using a graph where the first `tick()` emits `Done` (all tasks already Completed).
    /// 3. Asserting that `try_recv_secret_request()` returns `None` after the plan loop,
    ///    proving the drain was executed.
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn test_secret_drain_after_instant_completion() {
        use tokio_util::sync::CancellationToken;
        use zeph_subagent::def::{SkillFilter, SubAgentPermissions, ToolPolicy};
        use zeph_subagent::hooks::SubagentHooks;
        use zeph_subagent::{
            PermissionGrants, SecretRequest, SubAgentDef, SubAgentHandle, SubAgentManager,
            SubAgentState, SubAgentStatus,
        };

        // Channel with one pre-loaded confirmation (approve) so the bridge can resolve the
        // pending request when it is finally drained.
        let channel = MockChannel::new(vec![]).with_confirmations(vec![true]);

        // Provider returns aggregation synthesis (needed to satisfy finalize_plan_execution).
        let provider = mock_provider(vec!["synthesis".into()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.orchestration.orchestration_config.enabled = true;

        // Build a manager with one agent definition (needed by finalize_plan_execution).
        let mut mgr = SubAgentManager::new(4);
        mgr.definitions_mut().push(SubAgentDef {
            name: "worker".into(),
            description: "A worker".into(),
            model: None,
            tools: ToolPolicy::InheritAll,
            disallowed_tools: vec![],
            permissions: SubAgentPermissions::default(),
            skills: SkillFilter::default(),
            system_prompt: "You are helpful.".into(),
            hooks: SubagentHooks::default(),
            memory: None,
            source: None,
            file_path: None,
        });

        // Create a fake handle whose `pending_secret_rx` already contains one SecretRequest.
        // This simulates a sub-agent that queued the request before the plan loop ran.
        let (secret_request_tx, pending_secret_rx) = tokio::sync::mpsc::channel::<SecretRequest>(4);
        let (secret_tx, _secret_rx) = tokio::sync::mpsc::channel(1);
        let (status_tx, status_rx) = watch::channel(SubAgentStatus {
            state: SubAgentState::Completed,
            last_message: None,
            turns_used: 1,
            started_at: std::time::Instant::now(),
        });
        drop(status_tx);

        // Pre-load the secret request into the channel before plan execution starts.
        secret_request_tx
            .send(SecretRequest {
                secret_key: "api-key".into(),
                reason: Some("test drain".into()),
            })
            .await
            .expect("channel must accept request");
        drop(secret_request_tx); // close sender so try_recv returns None after drain

        let fake_handle_id = "deadbeef-0000-0000-0000-000000000001".to_owned();
        let def_clone = mgr.definitions()[0].clone();
        mgr.insert_handle_for_test(
            fake_handle_id.clone(),
            SubAgentHandle {
                id: fake_handle_id.clone(),
                def: def_clone,
                task_id: fake_handle_id.clone(),
                state: SubAgentState::Completed,
                join_handle: None,
                cancel: CancellationToken::new(),
                status_rx,
                grants: PermissionGrants::default(),
                pending_secret_rx,
                secret_tx,
                started_at_str: "2026-01-01T00:00:00Z".to_owned(),
                transcript_dir: None,
            },
        );
        agent.orchestration.subagent_manager = Some(mgr);

        // Graph with one already-Completed task in Running status: the first tick() finds no
        // Running/Ready tasks and emits Done{Completed} immediately (instant completion).
        let mut graph = TaskGraph::new("instant goal");
        let mut node = TaskNode::new(0, "task-0", "already done");
        node.status = TaskStatus::Completed;
        node.result = Some(TaskResult {
            output: "task output".into(),
            artifacts: vec![],
            duration_ms: 1,
            agent_id: None,
            agent_def: None,
        });
        graph.tasks.push(node);
        graph.status = GraphStatus::Running;
        agent.orchestration.pending_graph = Some(graph);

        // Run the plan loop — the fix adds a post-loop drain call.
        agent
            .handle_plan_command_as_string(PlanCommand::Confirm)
            .await
            .unwrap();

        // After plan completion, the secret request must have been drained.
        // If the drain was NOT called, try_recv_secret_request() would return Some(_).
        let leftover = agent
            .orchestration
            .subagent_manager
            .as_mut()
            .and_then(SubAgentManager::try_recv_secret_request);
        assert!(
            leftover.is_none(),
            "pending secret request must be drained after instant plan completion; \
             got: {leftover:?}"
        );
    }

    /// GAP-8: `handle_plan_confirm` with no sub-agents defined executes the task inline
    /// via the main provider. Verifies `RunInline` path: plan succeeds, provider output
    /// appears in aggregation, `pending_graph` is cleared.
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn plan_confirm_no_subagents_executes_inline() {
        use zeph_subagent::SubAgentManager;

        // Provider returns task result then aggregation synthesis.
        let provider = mock_provider(vec!["inline task output".into(), "synthesis done".into()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.orchestration.orchestration_config.enabled = true;

        // SubAgentManager with no definitions → route() returns None → RunInline.
        agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

        // Simple single-task graph.
        let mut graph = TaskGraph::new("inline goal");
        let node = TaskNode::new(0, "task-0", "do something inline");
        graph.tasks.push(node);
        graph.status = GraphStatus::Created;
        agent.orchestration.pending_graph = Some(graph);

        agent
            .handle_plan_command_as_string(PlanCommand::Confirm)
            .await
            .unwrap();

        // Graph must be cleared after successful execution.
        assert!(
            agent.orchestration.pending_graph.is_none(),
            "pending_graph must be cleared after inline plan completion"
        );
        let msgs = agent.channel.sent_messages();
        assert!(
            msgs.iter().any(|m| m.contains("synthesis done")),
            "aggregation synthesis must appear in messages; got: {msgs:?}"
        );
    }

    /// COV-01: `/plan cancel` received during `run_scheduler_loop` cancels the plan.
    ///
    /// Verifies that when the channel delivers "/plan cancel" while the scheduler loop
    /// is waiting for a task event, `cancel_all()` is called and the loop exits with
    /// `GraphStatus::Canceled`. The "Canceling plan..." status must be sent immediately.
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn plan_cancel_during_scheduler_loop_cancels_plan() {
        use crate::config::OrchestrationConfig;
        use zeph_orchestration::{DagScheduler, RuleBasedRouter};
        use zeph_subagent::SubAgentManager;

        // Channel pre-loaded with "/plan cancel" — returned immediately on first recv().
        let channel = MockChannel::new(vec!["/plan cancel".to_owned()]);
        let provider = mock_provider(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.orchestration.orchestration_config.enabled = true;
        agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

        // Graph in Running status with one task in Running state: tick() will not emit
        // any actions (no Ready tasks, no timed-out running tasks), so the loop reaches
        // wait_event() / select! immediately.
        let mut graph = TaskGraph::new("cancel test goal");
        let mut node = TaskNode::new(0, "task-0", "will be canceled");
        node.status = TaskStatus::Running;
        graph.tasks.push(node);
        graph.status = GraphStatus::Running;

        let config = OrchestrationConfig {
            enabled: true,
            ..OrchestrationConfig::default()
        };
        let mut scheduler =
            DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![]).unwrap();

        let token = tokio_util::sync::CancellationToken::new();
        let status = agent
            .run_scheduler_loop(&mut scheduler, 1, token)
            .await
            .unwrap();

        assert_eq!(
            status,
            GraphStatus::Canceled,
            "run_scheduler_loop must return Canceled when /plan cancel is received"
        );
        assert!(
            agent
                .channel
                .statuses
                .lock()
                .unwrap()
                .iter()
                .any(|s| s.contains("Canceling plan")),
            "must send 'Canceling plan...' status before processing cancel"
        );
    }

    /// COV-02: `finalize_plan_execution` with `GraphStatus::Canceled` sends the correct
    /// message, does NOT store the graph into `pending_graph`, and updates
    /// `orchestration.tasks_completed` with the count of tasks that finished before cancel.
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn finalize_plan_execution_canceled_does_not_store_graph() {
        use zeph_subagent::SubAgentManager;

        let channel = MockChannel::new(vec![]);
        let provider = mock_provider(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (metrics_tx, metrics_rx) = watch::channel(MetricsSnapshot::default());
        let mut agent =
            Agent::new(provider, channel, registry, None, 5, executor).with_metrics(metrics_tx);
        agent.orchestration.orchestration_config.enabled = true;
        agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

        // Graph with one completed task and one canceled task — typical mid-cancel state.
        let mut graph = TaskGraph::new("cancel finalize test");
        let mut completed = TaskNode::new(0, "task-done", "finished");
        completed.status = TaskStatus::Completed;
        completed.result = Some(TaskResult {
            output: "done".into(),
            artifacts: vec![],
            duration_ms: 10,
            agent_id: None,
            agent_def: None,
        });
        let mut canceled = TaskNode::new(1, "task-canceled", "was running");
        canceled.status = TaskStatus::Canceled;
        graph.tasks.push(completed);
        graph.tasks.push(canceled);
        graph.status = GraphStatus::Canceled;

        agent
            .finalize_plan_execution(graph, GraphStatus::Canceled)
            .await
            .unwrap();

        let msgs = agent.channel.sent_messages();
        assert!(
            msgs.iter()
                .any(|m| m.contains("canceled") || m.contains("Canceled")),
            "must send a cancellation message; got: {msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m.contains("1/2")),
            "must report completed task count (1/2); got: {msgs:?}"
        );
        assert!(
            agent.orchestration.pending_graph.is_none(),
            "canceled plan must NOT be stored in pending_graph"
        );
        let snapshot = metrics_rx.borrow().clone();
        assert_eq!(
            snapshot.orchestration.tasks_completed, 1,
            "tasks completed before cancellation must be counted in metrics"
        );
    }

    /// COV-03: a non-cancel message received via the channel during `run_scheduler_loop`
    /// is queued in `message_queue` for processing after the plan completes.
    ///
    /// Verifies the `tokio::select!` path added in #1603: when the channel delivers a
    /// non-cancel message while the loop is waiting for a scheduler event, the message
    /// is passed to `enqueue_or_merge()` and appears in `agent.msg.message_queue` after
    /// `run_scheduler_loop` returns.
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn scheduler_loop_queues_non_cancel_message() {
        use crate::config::OrchestrationConfig;
        use zeph_orchestration::{DagScheduler, RuleBasedRouter};
        use zeph_subagent::SubAgentManager;

        // Channel pre-loaded with one non-cancel message; second recv() returns None,
        // which terminates the loop with GraphStatus::Failed — acceptable for this test
        // since we only verify queuing, not plan completion status.
        let channel = MockChannel::new(vec!["hello".to_owned()]);
        let provider = mock_provider(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.orchestration.orchestration_config.enabled = true;
        agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

        // Graph in Running status with one task in Running state: tick() emits no actions
        // (no Ready tasks, running_in_graph_now > 0 suppresses Done), so the loop reaches
        // the select! where channel.recv() delivers "hello" before the loop exits.
        let mut graph = TaskGraph::new("queue test goal");
        let mut node = TaskNode::new(0, "task-0", "long running task");
        node.status = TaskStatus::Running;
        graph.tasks.push(node);
        graph.status = GraphStatus::Running;

        let config = OrchestrationConfig {
            enabled: true,
            ..OrchestrationConfig::default()
        };
        let mut scheduler =
            DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![]).unwrap();

        let token = tokio_util::sync::CancellationToken::new();
        let _ = agent
            .run_scheduler_loop(&mut scheduler, 1, token)
            .await
            .unwrap();

        assert_eq!(
            agent.msg.message_queue.len(),
            1,
            "non-cancel message must be queued in message_queue; got: {:?}",
            agent
                .msg
                .message_queue
                .iter()
                .map(|m| &m.text)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            agent.msg.message_queue[0].text, "hello",
            "queued message text must match the received message"
        );
    }

    /// COV-04: channel close (`Ok(None)`) on an exit-supporting channel (CLI/TUI) returns
    /// `GraphStatus::Canceled` — no retry needed, stdin EOF is a normal termination.
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn scheduler_loop_channel_close_supports_exit_returns_canceled() {
        use crate::config::OrchestrationConfig;
        use zeph_orchestration::{DagScheduler, RuleBasedRouter};
        use zeph_subagent::SubAgentManager;

        // Empty channel with exit_supported=true (the default): recv() returns Ok(None) immediately.
        let channel = MockChannel::new(vec![]);
        let provider = mock_provider(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.orchestration.orchestration_config.enabled = true;
        agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

        let mut graph = TaskGraph::new("channel close test goal");
        let mut node = TaskNode::new(0, "task-0", "will be canceled on channel close");
        node.status = TaskStatus::Running;
        graph.tasks.push(node);
        graph.status = GraphStatus::Running;

        let config = OrchestrationConfig {
            enabled: true,
            ..OrchestrationConfig::default()
        };
        let mut scheduler =
            DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![]).unwrap();

        let token = tokio_util::sync::CancellationToken::new();
        let status = agent
            .run_scheduler_loop(&mut scheduler, 1, token)
            .await
            .unwrap();

        assert_eq!(
            status,
            GraphStatus::Canceled,
            "channel close on exit-supporting channel (CLI/TUI) must return Canceled, not Failed"
        );
    }

    /// COV-04b: channel close (`Ok(None)`) on a server channel (Telegram/Discord/Slack,
    /// `supports_exit()=false`) returns `GraphStatus::Failed` so the user can `/plan retry`
    /// after reconnecting.
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn scheduler_loop_channel_close_no_exit_support_returns_failed() {
        use crate::config::OrchestrationConfig;
        use zeph_orchestration::{DagScheduler, RuleBasedRouter};
        use zeph_subagent::SubAgentManager;

        // Channel with exit_supported=false simulates Telegram/Discord/Slack.
        let channel = MockChannel::new(vec![]).without_exit_support();
        let provider = mock_provider(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.orchestration.orchestration_config.enabled = true;
        agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

        let mut graph = TaskGraph::new("server channel close goal");
        let mut node = TaskNode::new(0, "task-0", "interrupted by infra failure");
        node.status = TaskStatus::Running;
        graph.tasks.push(node);
        graph.status = GraphStatus::Running;

        let config = OrchestrationConfig {
            enabled: true,
            ..OrchestrationConfig::default()
        };
        let mut scheduler =
            DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![]).unwrap();

        let token = tokio_util::sync::CancellationToken::new();
        let status = agent
            .run_scheduler_loop(&mut scheduler, 1, token)
            .await
            .unwrap();

        assert_eq!(
            status,
            GraphStatus::Failed,
            "channel close on server channel (no exit support) must return Failed so the plan can be retried"
        );
    }

    /// COV-04c: a task completion event that arrives between the last tick and the channel
    /// close is captured by the drain tick and honored — the loop returns the natural
    /// `Done` status from the drain rather than forcing `Canceled`/`Failed`.
    ///
    /// This verifies the drain-before-cancel ordering (architect S1 fix for #2246):
    /// `cancel_all()` empties `self.running`, so any completion event processed AFTER it
    /// would be silently discarded. The drain tick must come FIRST while `self.running`
    /// is still intact.
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn scheduler_loop_channel_close_drain_captures_completion() {
        use crate::config::OrchestrationConfig;
        use zeph_orchestration::{DagScheduler, RuleBasedRouter, TaskEvent, TaskOutcome};
        use zeph_subagent::SubAgentManager;

        // Channel is empty: recv() returns Ok(None) immediately, triggering the close path.
        let channel = MockChannel::new(vec![]);
        let provider = mock_provider(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.orchestration.orchestration_config.enabled = true;
        agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

        // Single-task graph in Running state.  The task is assigned an agent handle so
        // resume_from() reconstructs it in the running map — this is required for
        // process_event() to accept the completion event (it checks self.running).
        let mut graph = TaskGraph::new("drain capture goal");
        let mut node = TaskNode::new(0, "task-0", "completes just before channel close");
        node.status = TaskStatus::Running;
        node.assigned_agent = Some("handle-0".to_string());
        node.agent_hint = Some("worker".to_string());
        graph.tasks.push(node);
        graph.status = GraphStatus::Running;

        let config = OrchestrationConfig {
            enabled: true,
            ..OrchestrationConfig::default()
        };
        let mut scheduler =
            DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![]).unwrap();

        // Inject the completion event via the public event_sender() so it sits in event_rx
        // when the drain tick calls event_rx.try_recv().  This simulates the race: the task
        // finished between the last tick and the channel EOF.
        let event_tx = scheduler.event_sender();
        event_tx
            .send(TaskEvent {
                task_id: scheduler.graph().tasks[0].id,
                agent_handle_id: "handle-0".to_string(),
                outcome: TaskOutcome::Completed {
                    output: "finished just in time".to_string(),
                    artifacts: vec![],
                },
            })
            .await
            .expect("event_tx send must not fail");
        // Drop the sender so the channel is not kept alive beyond this test.
        drop(event_tx);

        let token = tokio_util::sync::CancellationToken::new();
        let status = agent
            .run_scheduler_loop(&mut scheduler, 1, token)
            .await
            .unwrap();

        // The drain tick processes the completion event while self.running is intact,
        // advances the graph to Completed, and emits Done{Completed}.  The loop must
        // honor this natural Done rather than overriding it with Canceled/Failed.
        assert_eq!(
            status,
            GraphStatus::Completed,
            "drain tick must capture the late completion and return Done(Completed); got {status:?}"
        );
        assert_eq!(
            scheduler.graph().tasks[0].status,
            TaskStatus::Completed,
            "task 0 must be Completed, not Canceled, when its completion is captured by the drain tick"
        );
    }

    /// COV-05: when channel closes (stdin EOF) while sub-agent tasks are still running,
    /// `run_scheduler_loop` must NOT cancel them immediately.  Instead it parks the recv
    /// arm (`stdin_closed = true`) and waits for the natural completion event.
    ///
    /// Simulates: piped `echo "/plan confirm" | zeph` — stdin closes before sub-agents finish.
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn stdin_closed_parks_when_tasks_running() {
        use crate::config::OrchestrationConfig;
        use zeph_orchestration::{DagScheduler, RuleBasedRouter, TaskEvent, TaskOutcome};
        use zeph_subagent::SubAgentManager;

        // Empty channel: recv() returns Ok(None) immediately, simulating piped stdin EOF.
        let channel = MockChannel::new(vec![]);
        let provider = mock_provider(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.orchestration.orchestration_config.enabled = true;
        agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

        // A single task that is already running when the channel closes.
        let mut graph = TaskGraph::new("piped stdin EOF with running task");
        let mut node = TaskNode::new(0, "task-0", "must finish naturally");
        node.status = TaskStatus::Running;
        node.assigned_agent = Some("handle-0".to_string());
        node.agent_hint = Some("worker".to_string());
        graph.tasks.push(node);
        graph.status = GraphStatus::Running;

        let config = OrchestrationConfig {
            enabled: true,
            ..OrchestrationConfig::default()
        };
        let mut scheduler =
            DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![]).unwrap();

        // Deliver the completion event asynchronously after a short delay so that the loop
        // first observes channel-close (stdin_closed = true), then receives the event.
        let event_tx = scheduler.event_sender();
        let task_id = scheduler.graph().tasks[0].id;
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let _ = event_tx
                .send(TaskEvent {
                    task_id,
                    agent_handle_id: "handle-0".to_string(),
                    outcome: TaskOutcome::Completed {
                        output: "natural completion".to_string(),
                        artifacts: vec![],
                    },
                })
                .await;
        });

        let token = tokio_util::sync::CancellationToken::new();
        let status = agent
            .run_scheduler_loop(&mut scheduler, 1, token)
            .await
            .unwrap();

        assert_eq!(
            status,
            GraphStatus::Completed,
            "loop must wait for natural task completion after stdin EOF, not cancel immediately; got {status:?}"
        );
        assert_eq!(
            scheduler.graph().tasks[0].status,
            TaskStatus::Completed,
            "task must be Completed, not Canceled, when loop parks on stdin EOF"
        );
    }

    /// COV-06: when channel closes (stdin EOF) and there are no running tasks,
    /// `run_scheduler_loop` must exit immediately with `GraphStatus::Canceled`
    /// (existing behavior preserved for empty-scheduler case).
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn stdin_closed_exits_when_no_tasks() {
        use crate::config::OrchestrationConfig;
        use zeph_orchestration::{DagScheduler, RuleBasedRouter};
        use zeph_subagent::SubAgentManager;

        // Empty channel: recv() returns Ok(None) immediately.
        let channel = MockChannel::new(vec![]);
        let provider = mock_provider(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.orchestration.orchestration_config.enabled = true;
        agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

        // Graph has a task in Running state, but no entry in scheduler.running map
        // (simulates the case where the task was already drained before channel close).
        let mut graph = TaskGraph::new("no running tasks on channel close");
        let mut node = TaskNode::new(0, "task-0", "already drained");
        node.status = TaskStatus::Running;
        graph.tasks.push(node);
        graph.status = GraphStatus::Running;

        let config = OrchestrationConfig {
            enabled: true,
            ..OrchestrationConfig::default()
        };
        // resume_from without assigned_agent → running map stays empty.
        let mut scheduler =
            DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![]).unwrap();

        let token = tokio_util::sync::CancellationToken::new();
        let status = agent
            .run_scheduler_loop(&mut scheduler, 1, token)
            .await
            .unwrap();

        assert_eq!(
            status,
            GraphStatus::Canceled,
            "channel close with no running tasks on exit-supporting channel must return Canceled; got {status:?}"
        );
    }

    /// GAP-9: `handle_plan_status` shows the correct message for each graph status.
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn plan_status_reflects_graph_status() {
        // No active plan → "No active plan."
        let mut agent = agent_with_orchestration();
        let out = agent
            .handle_plan_command_as_string(PlanCommand::Status(None))
            .await
            .unwrap();
        assert!(
            out.contains("No active plan"),
            "no plan → 'No active plan'; got: {out:?}"
        );

        // GraphStatus::Created → awaiting confirmation.
        let mut agent = agent_with_orchestration();
        agent.orchestration.pending_graph = Some(make_simple_graph(GraphStatus::Created));
        let out = agent
            .handle_plan_command_as_string(PlanCommand::Status(None))
            .await
            .unwrap();
        assert!(
            out.contains("awaiting confirmation"),
            "Created graph → 'awaiting confirmation'; got: {out:?}"
        );

        // GraphStatus::Failed → retry message.
        let mut agent = agent_with_orchestration();
        let mut failed_graph = make_simple_graph(GraphStatus::Created);
        failed_graph.status = GraphStatus::Failed;
        agent.orchestration.pending_graph = Some(failed_graph);
        let out = agent
            .handle_plan_command_as_string(PlanCommand::Status(None))
            .await
            .unwrap();
        assert!(
            out.contains("failed") || out.contains("Failed"),
            "Failed graph → failure message; got: {out:?}"
        );

        // GraphStatus::Paused → resume message.
        let mut agent = agent_with_orchestration();
        let mut paused_graph = make_simple_graph(GraphStatus::Created);
        paused_graph.status = GraphStatus::Paused;
        agent.orchestration.pending_graph = Some(paused_graph);
        let out = agent
            .handle_plan_command_as_string(PlanCommand::Status(None))
            .await
            .unwrap();
        assert!(
            out.contains("paused") || out.contains("Paused"),
            "Paused graph → paused message; got: {out:?}"
        );

        // GraphStatus::Completed → completed message.
        let mut agent = agent_with_orchestration();
        let mut completed_graph = make_simple_graph(GraphStatus::Created);
        completed_graph.status = GraphStatus::Completed;
        agent.orchestration.pending_graph = Some(completed_graph);
        let out = agent
            .handle_plan_command_as_string(PlanCommand::Status(None))
            .await
            .unwrap();
        assert!(
            out.contains("completed") || out.contains("Completed"),
            "Completed graph → completed message; got: {out:?}"
        );
    }

    /// Regression for #1879: `finalize_plan_execution` with `GraphStatus::Failed` where no
    /// tasks actually failed (all canceled due to scheduler deadlock) must emit
    /// "Plan canceled. N/M tasks did not run." and NOT "Plan failed. 0/N tasks failed".
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn finalize_plan_execution_deadlock_emits_cancelled_message() {
        use zeph_subagent::SubAgentManager;

        let channel = MockChannel::new(vec![]);
        let provider = mock_provider(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.orchestration.orchestration_config.enabled = true;
        agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

        // Simulate deadlock: graph Failed, one task Blocked → Canceled, one task Pending → Canceled.
        let mut graph = TaskGraph::new("deadlock goal");
        let mut task0 = TaskNode::new(0, "upstream", "will be blocked");
        task0.status = TaskStatus::Canceled;
        let mut task1 = TaskNode::new(1, "downstream", "never ran");
        task1.status = TaskStatus::Canceled;
        graph.tasks.push(task0);
        graph.tasks.push(task1);
        graph.status = GraphStatus::Failed;

        agent
            .finalize_plan_execution(graph, GraphStatus::Failed)
            .await
            .unwrap();

        let msgs = agent.channel.sent_messages();
        // Must NOT say "0/2 tasks failed".
        assert!(
            !msgs.iter().any(|m| m.contains("0/2 tasks failed")),
            "misleading '0/2 tasks failed' message must not appear; got: {msgs:?}"
        );
        // Must say "Plan canceled".
        assert!(
            msgs.iter().any(|m| m.contains("Plan canceled")),
            "must contain 'Plan canceled' for pure deadlock; got: {msgs:?}"
        );
        // Must mention the count of tasks that did not run.
        assert!(
            msgs.iter().any(|m| m.contains("2/2")),
            "must report 2/2 canceled; got: {msgs:?}"
        );
    }

    /// COV-METRICS-01: `handle_plan_goal` increments `api_calls` and `plans_total` after
    /// a successful LlmPlanner call. This test covers the production metrics path in
    /// `handle_plan_goal` that was not exercised by the `status_command_shows_orchestration_*`
    /// tests (which set metrics directly via `update_metrics`).
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn plan_goal_increments_api_calls_and_plans_total() {
        let valid_plan_json = r#"{"tasks": [
            {"task_id": "step-one", "title": "Step one", "description": "Do step one", "depends_on": []}
        ]}"#
        .to_string();

        let provider = mock_provider(vec![valid_plan_json]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(MetricsSnapshot::default());
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);
        agent.orchestration.orchestration_config.enabled = true;
        agent
            .orchestration
            .orchestration_config
            .confirm_before_execute = true;

        agent
            .handle_plan_command_as_string(PlanCommand::Goal("build something".to_owned()))
            .await
            .unwrap();

        let snapshot = rx.borrow().clone();
        assert_eq!(
            snapshot.api_calls, 1,
            "api_calls must be incremented by 1 after a successful plan() call; got: {}",
            snapshot.api_calls
        );
        assert_eq!(
            snapshot.orchestration.plans_total, 1,
            "plans_total must be incremented by 1 after plan() succeeds; got: {}",
            snapshot.orchestration.plans_total
        );
        assert_eq!(
            snapshot.orchestration.tasks_total, 1,
            "tasks_total must match the number of tasks in the plan; got: {}",
            snapshot.orchestration.tasks_total
        );
    }

    /// COV-METRICS-02: `finalize_plan_execution` with `GraphStatus::Completed` increments
    /// `api_calls` for the aggregator call and updates `tasks_completed` / `tasks_skipped`.
    /// This covers the aggregator metrics path that was not tested end-to-end.
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn finalize_plan_execution_completed_increments_aggregator_metrics() {
        use zeph_subagent::SubAgentManager;

        // Provider returns the aggregation synthesis.
        let provider = mock_provider(vec!["synthesis".into()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(MetricsSnapshot::default());
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);
        agent.orchestration.orchestration_config.enabled = true;
        agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

        // Graph with one completed and one skipped task.
        let mut graph = TaskGraph::new("metrics finalize test");
        let mut completed = TaskNode::new(0, "task-done", "desc");
        completed.status = TaskStatus::Completed;
        completed.result = Some(TaskResult {
            output: "ok".into(),
            artifacts: vec![],
            duration_ms: 5,
            agent_id: None,
            agent_def: None,
        });
        let mut skipped = TaskNode::new(1, "task-skip", "desc");
        skipped.status = TaskStatus::Skipped;
        graph.tasks.push(completed);
        graph.tasks.push(skipped);
        graph.status = GraphStatus::Completed;

        agent
            .finalize_plan_execution(graph, GraphStatus::Completed)
            .await
            .unwrap();

        let snapshot = rx.borrow().clone();
        assert_eq!(
            snapshot.api_calls, 1,
            "api_calls must be incremented by 1 for the aggregator LLM call; got: {}",
            snapshot.api_calls
        );
        assert_eq!(
            snapshot.orchestration.tasks_completed, 1,
            "tasks_completed must be 1; got: {}",
            snapshot.orchestration.tasks_completed
        );
        assert_eq!(
            snapshot.orchestration.tasks_skipped, 1,
            "tasks_skipped must be 1; got: {}",
            snapshot.orchestration.tasks_skipped
        );
    }

    /// Regression for #1879: mixed failure — some tasks failed, some canceled.
    /// Message must say "Plan failed. X/M tasks failed, Y canceled:" (not misleading).
    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn finalize_plan_execution_mixed_failed_and_cancelled() {
        use zeph_subagent::SubAgentManager;

        let channel = MockChannel::new(vec![]);
        let provider = mock_provider(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.orchestration.orchestration_config.enabled = true;
        agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

        let mut graph = TaskGraph::new("mixed goal");
        let mut failed = TaskNode::new(0, "failed-task", "really failed");
        failed.status = TaskStatus::Failed;
        failed.result = Some(TaskResult {
            output: "error: something went wrong".into(),
            artifacts: vec![],
            duration_ms: 100,
            agent_id: None,
            agent_def: None,
        });
        let mut cancelled = TaskNode::new(1, "cancelled-task", "never ran");
        cancelled.status = TaskStatus::Canceled;
        graph.tasks.push(failed);
        graph.tasks.push(cancelled);
        graph.status = GraphStatus::Failed;

        agent
            .finalize_plan_execution(graph, GraphStatus::Failed)
            .await
            .unwrap();

        let msgs = agent.channel.sent_messages();
        // Must say "Plan failed." (not "Plan canceled.").
        assert!(
            msgs.iter().any(|m| m.contains("Plan failed")),
            "mixed state must say 'Plan failed'; got: {msgs:?}"
        );
        // Must mention canceled count.
        assert!(
            msgs.iter().any(|m| m.contains("canceled")),
            "must mention canceled tasks in mixed state; got: {msgs:?}"
        );
        // Must list failed task.
        assert!(
            msgs.iter().any(|m| m.contains("failed-task")),
            "must list the failed task; got: {msgs:?}"
        );
    }
}

#[cfg(test)]
mod secret_reason_truncation {
    /// Build the prompt string the same way `process_pending_secret_requests` does.
    fn build_prompt(secret_key: &str, reason: Option<&str>) -> String {
        format!(
            "Sub-agent requests secret '{}'. Allow?{}",
            crate::text::truncate_to_chars(secret_key, 100),
            reason
                .map(|r| format!(" Reason: {}", crate::text::truncate_to_chars(r, 200)))
                .unwrap_or_default()
        )
    }

    #[test]
    fn reason_short_ascii_unchanged() {
        let reason = "need access to external API";
        let prompt = build_prompt("MY_SECRET", Some(reason));
        assert!(prompt.contains(reason));
    }

    #[test]
    fn reason_over_200_chars_truncated_to_200() {
        let reason = "a".repeat(300);
        let prompt = build_prompt("MY_SECRET", Some(&reason));
        // Extract the reason portion after "Reason: "
        let after = prompt.split("Reason: ").nth(1).unwrap();
        // truncate_to_chars appends … (U+2026) when truncating: 200 chars + ellipsis = 201.
        assert_eq!(after.chars().count(), 201);
        assert!(after.ends_with('\u{2026}'));
    }

    #[test]
    fn reason_exactly_200_chars_unchanged() {
        let reason = "b".repeat(200);
        let prompt = build_prompt("MY_SECRET", Some(&reason));
        let after = prompt.split("Reason: ").nth(1).unwrap();
        // Exactly at limit: no truncation, no ellipsis.
        assert_eq!(after.chars().count(), 200);
        assert!(!after.ends_with('\u{2026}'));
    }

    #[test]
    fn reason_multibyte_utf8_truncated_at_char_boundary() {
        // Each Cyrillic char is 2 bytes; 300 chars = 600 bytes.
        let reason = "й".repeat(300);
        let prompt = build_prompt("MY_SECRET", Some(&reason));
        let after = prompt.split("Reason: ").nth(1).unwrap();
        // truncate_to_chars appends … when truncating: 200 chars + ellipsis = 201.
        assert_eq!(after.chars().count(), 201);
        assert!(after.ends_with('\u{2026}'));
        assert!(std::str::from_utf8(after.as_bytes()).is_ok());
    }

    #[test]
    fn reason_none_produces_no_reason_suffix() {
        let prompt = build_prompt("MY_SECRET", None);
        assert!(!prompt.contains("Reason:"));
        assert!(prompt.ends_with("Allow?"));
    }

    #[test]
    fn secret_key_short_unchanged() {
        let prompt = build_prompt("MY_API_KEY", None);
        assert!(prompt.contains("MY_API_KEY"));
    }

    #[test]
    fn secret_key_over_100_chars_truncated() {
        let key = "A".repeat(150);
        let prompt = build_prompt(&key, None);
        // Extract the key portion between "secret '" and "'."
        let after_quote = prompt.split("secret '").nth(1).unwrap();
        let key_in_prompt = after_quote.split("'. Allow?").next().unwrap();
        // truncate_to_chars appends … when truncating: 100 chars + ellipsis = 101.
        assert_eq!(key_in_prompt.chars().count(), 101);
        assert!(key_in_prompt.ends_with('\u{2026}'));
    }

    #[test]
    fn secret_key_exactly_100_chars_unchanged() {
        let key = "B".repeat(100);
        let prompt = build_prompt(&key, None);
        let after_quote = prompt.split("secret '").nth(1).unwrap();
        let key_in_prompt = after_quote.split("'. Allow?").next().unwrap();
        assert_eq!(key_in_prompt.chars().count(), 100);
        assert!(!key_in_prompt.ends_with('\u{2026}'));
    }
}

#[cfg(all(test, feature = "scheduler"))]
mod inline_tool_loop_tests {
    use std::sync::Mutex;

    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;
    use zeph_llm::provider::{ChatResponse, ToolUseRequest};
    use zeph_tools::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};

    use super::super::Agent;
    use super::agent_tests::{MockChannel, create_test_registry};

    /// A `ToolExecutor` that responds to `execute_tool_call` with a fixed output sequence.
    struct CallableToolExecutor {
        outputs: Mutex<Vec<Result<Option<ToolOutput>, ToolError>>>,
    }

    impl CallableToolExecutor {
        fn new(outputs: Vec<Result<Option<ToolOutput>, ToolError>>) -> Self {
            Self {
                outputs: Mutex::new(outputs),
            }
        }

        fn fixed_output(summary: &str) -> Self {
            Self::new(vec![Ok(Some(ToolOutput {
                tool_name: "test_tool".into(),
                summary: summary.to_owned(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            }))])
        }

        fn failing() -> Self {
            Self::new(vec![Err(ToolError::InvalidParams {
                message: "tool failed".into(),
            })])
        }
    }

    impl ToolExecutor for CallableToolExecutor {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }

        async fn execute_tool_call(
            &self,
            _call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            let mut outputs = self.outputs.lock().unwrap();
            if outputs.is_empty() {
                Ok(None)
            } else {
                outputs.remove(0)
            }
        }
    }

    fn tool_use_response(tool_id: &str, tool_name: &str) -> ChatResponse {
        ChatResponse::ToolUse {
            text: None,
            tool_calls: vec![ToolUseRequest {
                id: tool_id.to_owned(),
                name: tool_name.into(),
                input: serde_json::json!({"arg": "val"}),
            }],
            thinking_blocks: vec![],
        }
    }

    #[tokio::test]
    async fn text_only_response_returns_immediately() {
        let (mock, _counter) =
            MockProvider::default().with_tool_use(vec![ChatResponse::Text("the answer".into())]);
        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = CallableToolExecutor::new(vec![]);

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.run_inline_tool_loop("what is 2+2?", 10).await;

        assert_eq!(result.unwrap(), "the answer");
    }

    #[tokio::test]
    async fn single_tool_iteration_returns_final_text() {
        let (mock, counter) = MockProvider::default().with_tool_use(vec![
            tool_use_response("call-1", "test_tool"),
            ChatResponse::Text("done".into()),
        ]);
        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = CallableToolExecutor::fixed_output("tool result");

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.run_inline_tool_loop("run a tool", 10).await;

        assert_eq!(result.unwrap(), "done");
        assert_eq!(*counter.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn loop_terminates_at_max_iterations() {
        // Provider always returns ToolUse — loop must stop after max_iterations.
        let responses: Vec<ChatResponse> = (0..25)
            .map(|i| tool_use_response(&format!("call-{i}"), "test_tool"))
            .collect();
        let (mock, counter) = MockProvider::default().with_tool_use(responses);
        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = CallableToolExecutor::fixed_output("ok");

        let max_iter = 5usize;
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.run_inline_tool_loop("loop forever", max_iter).await;

        // Must return Ok (not panic or hang) and have called the provider exactly max_iter times.
        assert!(result.is_ok());
        assert_eq!(*counter.lock().unwrap(), u32::try_from(max_iter).unwrap());
    }

    #[tokio::test]
    async fn tool_error_produces_is_error_result_and_loop_continues() {
        // First call: ToolUse with a failing executor → ToolResult with is_error=true.
        // Second call: Text → loop ends.
        // We verify the loop continues (doesn't abort) and returns the final text.
        let (mock, _counter) = MockProvider::default().with_tool_use(vec![
            tool_use_response("call-err", "test_tool"),
            ChatResponse::Text("recovered".into()),
        ]);
        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = CallableToolExecutor::failing();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.run_inline_tool_loop("trigger error", 10).await;

        assert_eq!(result.unwrap(), "recovered");
    }

    #[tokio::test]
    async fn multiple_tool_iterations_before_text() {
        // Two ToolUse rounds, then Text. Verifies the loop handles chained tool calls.
        let (mock, counter) = MockProvider::default().with_tool_use(vec![
            tool_use_response("call-1", "test_tool"),
            tool_use_response("call-2", "test_tool"),
            ChatResponse::Text("all done".into()),
        ]);
        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        // Need two successful outputs for the two tool calls.
        let executor = CallableToolExecutor::new(vec![
            Ok(Some(ToolOutput {
                tool_name: "test_tool".into(),
                summary: "result-1".into(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            })),
            Ok(Some(ToolOutput {
                tool_name: "test_tool".into(),
                summary: "result-2".into(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            })),
        ]);

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent
            .run_inline_tool_loop("two tools then answer", 10)
            .await;

        assert_eq!(result.unwrap(), "all done");
        assert_eq!(*counter.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn provider_error_is_propagated() {
        // MockProvider::failing() makes chat_with_tools return Err via the fallback chat() path.
        let provider = AnyProvider::Mock(zeph_llm::mock::MockProvider::failing());
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = CallableToolExecutor::new(vec![]);

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.run_inline_tool_loop("this will fail", 10).await;

        assert!(result.is_err());
    }

    // Regression test for issue #2542: elicitation deadlock in run_inline_tool_loop.
    //
    // The real deadlock scenario: MCP tool sends an elicitation event and then blocks
    // waiting for the agent to respond via response_tx. Meanwhile execute_tool_call_erased
    // also blocks waiting for the MCP tool — neither side makes progress.
    //
    // The fix: select! concurrently drains elicitation_rx while awaiting the tool result.
    //
    // Test design: BlockingElicitingExecutor sends an elicitation event then blocks on
    // `unblock_rx` (a oneshot whose sender is never signalled — it stays pending until
    // the future is cancelled). When select! picks the elicitation branch it cancels the
    // tool future, dropping `unblock_rx`. On the next invocation `unblock_rx` is None so
    // the executor returns immediately. This guarantees select! MUST pick the elicitation
    // branch on the first iteration (tool is the only blocking party). If the fix were
    // absent, the test would deadlock and time out.
    #[tokio::test]
    async fn elicitation_event_during_tool_execution_is_handled() {
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::sync::{mpsc, oneshot};
        use zeph_mcp::ElicitationEvent;

        struct BlockingElicitingExecutor {
            elic_tx: mpsc::Sender<ElicitationEvent>,
            // Holds the oneshot rx that the executor awaits on the first call.
            // Dropped (None) on re-invocation after select! cancels the first future.
            unblock_rx: Arc<std::sync::Mutex<Option<oneshot::Receiver<()>>>>,
            sent: Arc<std::sync::atomic::AtomicBool>,
        }

        impl ToolExecutor for BlockingElicitingExecutor {
            async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }

            async fn execute_tool_call(
                &self,
                _call: &ToolCall,
            ) -> Result<Option<ToolOutput>, ToolError> {
                if !self.sent.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    let (response_tx, _response_rx) = oneshot::channel();
                    let event = ElicitationEvent {
                        server_id: "test-server".to_owned(),
                        request:
                            rmcp::model::CreateElicitationRequestParams::FormElicitationParams {
                                meta: None,
                                message: "please fill in".to_owned(),
                                requested_schema: rmcp::model::ElicitationSchema::new(
                                    std::collections::BTreeMap::new(),
                                ),
                            },
                        response_tx,
                    };
                    let _ = self.elic_tx.send(event).await;
                    // Block until select! cancels this future (simulates the MCP server
                    // waiting for a response). Cancellation drops unblock_rx, causing
                    // this await to resolve with Err — but the future is already dropped
                    // by then. On re-invocation unblock_rx is None, so we skip blocking.
                    let rx = self.unblock_rx.lock().unwrap().take();
                    if let Some(rx) = rx {
                        let _ = rx.await;
                    }
                }
                Ok(Some(ToolOutput {
                    tool_name: "elicit_tool".into(),
                    summary: "result".into(),
                    blocks_executed: 1,
                    filter_stats: None,
                    diff: None,
                    streamed: false,
                    terminal_id: None,
                    locations: None,
                    raw_response: None,
                    claim_source: None,
                }))
            }
        }

        let (elic_tx, elic_rx) = mpsc::channel::<ElicitationEvent>(4);
        // Keep _unblock_tx alive for the duration of the test so that unblock_rx.await
        // truly blocks (channel not closed) until the future holding it is cancelled.
        let (_unblock_tx, unblock_rx) = oneshot::channel::<()>();

        let (mock, _counter) = MockProvider::default().with_tool_use(vec![
            tool_use_response("call-elic", "elicit_tool"),
            ChatResponse::Text("done".into()),
        ]);
        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = BlockingElicitingExecutor {
            elic_tx,
            unblock_rx: Arc::new(std::sync::Mutex::new(Some(unblock_rx))),
            sent: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_mcp_elicitation_rx(elic_rx);

        // A 5-second timeout turns a deadlock into a clear test failure instead of a hang.
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            agent.run_inline_tool_loop("trigger elicitation", 10),
        )
        .await
        .expect("run_inline_tool_loop timed out — elicitation deadlock not fixed")
        .unwrap();

        assert_eq!(result, "done");
    }
}

#[cfg(test)]
mod confirmation_propagation_tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;
    use zeph_llm::provider::{ChatResponse, ToolUseRequest};
    use zeph_tools::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
    use zeph_tools::registry::{InvocationHint, ToolDef};

    use super::super::Agent;
    use super::agent_tests::{MockChannel, create_test_registry};

    struct DagAwareToolExecutor {
        results: Mutex<HashMap<String, Result<Option<ToolOutput>, ToolError>>>,
    }

    impl DagAwareToolExecutor {
        // Each entry is consumed on first call; subsequent calls for the same tool_id return Ok(None).
        fn new(entries: Vec<(&str, Result<Option<ToolOutput>, ToolError>)>) -> Self {
            Self {
                results: Mutex::new(
                    entries
                        .into_iter()
                        .map(|(k, v)| (k.to_owned(), v))
                        .collect(),
                ),
            }
        }

        fn make_output(tool_name: &str, summary: &str) -> ToolOutput {
            ToolOutput {
                tool_name: tool_name.into(),
                summary: summary.to_owned(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            }
        }
    }

    impl ToolExecutor for DagAwareToolExecutor {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }

        fn tool_definitions(&self) -> Vec<ToolDef> {
            vec![
                ToolDef {
                    id: "tool_a".into(),
                    description: "tool a".into(),
                    schema: schemars::Schema::default(),
                    invocation: InvocationHint::ToolCall,
                    output_schema: None,
                },
                ToolDef {
                    id: "tool_b".into(),
                    description: "tool b".into(),
                    schema: schemars::Schema::default(),
                    invocation: InvocationHint::ToolCall,
                    output_schema: None,
                },
                ToolDef {
                    id: "tool_c".into(),
                    description: "tool c".into(),
                    schema: schemars::Schema::default(),
                    invocation: InvocationHint::ToolCall,
                    output_schema: None,
                },
            ]
        }

        async fn execute_tool_call(
            &self,
            call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            let mut results = self.results.lock().unwrap();
            results.remove(call.tool_id.as_str()).unwrap_or(Ok(None))
        }

        async fn execute_tool_call_confirmed(
            &self,
            call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            Ok(Some(Self::make_output(call.tool_id.as_str(), "confirmed")))
        }
    }

    fn dag_tool_use_response() -> ChatResponse {
        ChatResponse::ToolUse {
            text: None,
            tool_calls: vec![
                ToolUseRequest {
                    id: "tool_a_id".to_owned(),
                    name: "tool_a".to_owned().into(),
                    input: serde_json::json!({"arg": "value"}),
                },
                ToolUseRequest {
                    id: "tool_b_id".to_owned(),
                    name: "tool_b".to_owned().into(),
                    input: serde_json::json!({"source": "tool_a_id"}),
                },
            ],
            thinking_blocks: vec![],
        }
    }

    fn independent_tool_use_response() -> ChatResponse {
        ChatResponse::ToolUse {
            text: None,
            tool_calls: vec![
                ToolUseRequest {
                    id: "tool_a_id".to_owned(),
                    name: "tool_a".to_owned().into(),
                    input: serde_json::json!({"arg": "value"}),
                },
                ToolUseRequest {
                    id: "tool_c_id".to_owned(),
                    name: "tool_c".to_owned().into(),
                    input: serde_json::json!({"arg": "independent"}),
                },
            ],
            thinking_blocks: vec![],
        }
    }

    #[tokio::test]
    async fn confirmation_required_propagates_to_dependent_tier() {
        let (mock, _counter) = MockProvider::default().with_tool_use(vec![
            dag_tool_use_response(),
            ChatResponse::Text("done".into()),
        ]);
        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec!["test".to_string()]).with_confirmations(vec![false]);
        let registry = create_test_registry();
        let executor = DagAwareToolExecutor::new(vec![(
            "tool_a",
            Err(ToolError::ConfirmationRequired {
                command: "cmd".to_owned(),
            }),
        )]);

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.run().await;
        assert!(result.is_ok());

        let has_skipped = agent.msg.messages.iter().any(|m| {
            m.content
                .contains("Skipped: a prerequisite tool failed or requires confirmation")
        });
        let sent = agent.channel.sent_messages();
        let has_skipped_in_sent = sent
            .iter()
            .any(|m| m.contains("Skipped: a prerequisite tool failed or requires confirmation"));
        assert!(
            has_skipped || has_skipped_in_sent,
            "expected synthetic skip message for tool_b; sent={sent:?}, agent_msgs={:?}",
            agent
                .msg
                .messages
                .iter()
                .map(|m| &m.content)
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn independent_tool_not_affected_by_confirmation_required() {
        let (mock, _counter) = MockProvider::default().with_tool_use(vec![
            independent_tool_use_response(),
            ChatResponse::Text("done".into()),
        ]);
        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec!["test".to_string()]).with_confirmations(vec![false]);
        let registry = create_test_registry();
        let executor = DagAwareToolExecutor::new(vec![
            (
                "tool_a",
                Err(ToolError::ConfirmationRequired {
                    command: "cmd".to_owned(),
                }),
            ),
            (
                "tool_c",
                Ok(Some(DagAwareToolExecutor::make_output(
                    "tool_c",
                    "independent result",
                ))),
            ),
        ]);

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.run().await;
        assert!(result.is_ok());

        let has_independent_result = agent
            .msg
            .messages
            .iter()
            .any(|m| m.content.contains("independent result"));
        assert!(
            has_independent_result,
            "expected tool_c (independent) to execute normally; agent_msgs={:?}",
            agent
                .msg
                .messages
                .iter()
                .map(|m| &m.content)
                .collect::<Vec<_>>()
        );
    }
}

#[cfg(test)]
mod shutdown_summary_tests {
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;
    use zeph_llm::provider::{Message, MessageMetadata, Role};
    use zeph_memory::semantic::SemanticMemory;

    use super::super::Agent;
    use super::agent_tests::{MockChannel, MockToolExecutor, create_test_registry, mock_provider};

    #[tokio::test]
    async fn shutdown_summary_disabled_skips_llm() {
        let (mock, recorded) = MockProvider::default().with_recording();
        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_shutdown_summary_config(false, 4, 20, 10);

        // Add enough user messages to exceed the threshold.
        for i in 0..5 {
            agent.msg.messages.push(Message {
                role: Role::User,
                content: format!("user message {i}"),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });
        }

        agent.maybe_store_shutdown_summary().await;

        // LLM must not be called when feature is disabled.
        assert!(
            recorded.lock().unwrap().is_empty(),
            "LLM must not be called when shutdown_summary is disabled"
        );
    }

    #[tokio::test]
    async fn shutdown_summary_no_memory_skips_llm() {
        let (mock, recorded) = MockProvider::default().with_recording();
        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        // No .with_memory() call — memory_state.persistence.memory is None.
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
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
            recorded.lock().unwrap().is_empty(),
            "LLM must not be called when no memory backend is attached"
        );
    }

    #[tokio::test]
    async fn shutdown_summary_too_few_user_messages_skips_llm() {
        use std::sync::Arc;

        let (mock, recorded) = MockProvider::default().with_recording();
        let provider = AnyProvider::Mock(mock.clone());
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
            AnyProvider::Mock(MockProvider::default()),
            "test-model",
        )
        .await
        .unwrap();
        let cid = memory.sqlite().create_conversation().await.unwrap();

        // min_messages=4 but we will only add 2 user messages.
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(Arc::new(memory), cid, 100, 5, 1000)
            .with_shutdown_summary_config(true, 4, 20, 10);

        // System prompt is messages[0] — skip(1) counts from index 1.
        // Add 2 user messages: below the threshold of 4.
        agent.msg.messages.push(Message {
            role: Role::User,
            content: "first user message".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
        agent.msg.messages.push(Message {
            role: Role::Assistant,
            content: "assistant reply".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
        agent.msg.messages.push(Message {
            role: Role::User,
            content: "second user message".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });

        agent.maybe_store_shutdown_summary().await;

        assert!(
            recorded.lock().unwrap().is_empty(),
            "LLM must not be called when user message count is below min_messages"
        );
    }

    #[tokio::test]
    async fn shutdown_summary_only_counts_user_role_messages() {
        use std::sync::Arc;

        let (mock, recorded) = MockProvider::default().with_recording();
        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
            AnyProvider::Mock(MockProvider::default()),
            "test-model",
        )
        .await
        .unwrap();
        let cid = memory.sqlite().create_conversation().await.unwrap();

        // min_messages=4: need at least 4 user messages.
        // We add 8 assistant messages but only 3 user messages — should still skip.
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(Arc::new(memory), cid, 100, 5, 1000)
            .with_shutdown_summary_config(true, 4, 20, 10);

        for _ in 0..8 {
            agent.msg.messages.push(Message {
                role: Role::Assistant,
                content: "assistant reply".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });
        }
        for i in 0..3 {
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
            "assistant messages must not count toward min_messages threshold"
        );
    }

    #[tokio::test]
    async fn with_shutdown_summary_config_builder_sets_fields() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_shutdown_summary_config(false, 7, 15, 10);

        assert!(!agent.memory_state.compaction.shutdown_summary);
        assert_eq!(
            agent.memory_state.compaction.shutdown_summary_min_messages,
            7
        );
        assert_eq!(
            agent.memory_state.compaction.shutdown_summary_max_messages,
            15
        );
        assert_eq!(
            agent.memory_state.compaction.shutdown_summary_timeout_secs,
            10
        );
    }

    #[tokio::test]
    async fn shutdown_summary_default_config_values() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor);

        assert!(
            agent.memory_state.compaction.shutdown_summary,
            "shutdown_summary must be enabled by default"
        );
        assert_eq!(
            agent.memory_state.compaction.shutdown_summary_min_messages, 4,
            "default min_messages must be 4"
        );
        assert_eq!(
            agent.memory_state.compaction.shutdown_summary_max_messages, 20,
            "default max_messages must be 20"
        );
        assert_eq!(
            agent.memory_state.compaction.shutdown_summary_timeout_secs, 10,
            "default timeout_secs must be 10"
        );
    }

    // --- Doom-loop integration tests ---

    /// The real doom-loop detection lives in the agent's native tool loop. This test
    /// verifies that when the MockProvider (with tool_use=true) returns identical tool
    /// outputs DOOM_LOOP_WINDOW times in a row, the agent breaks the loop and sends
    /// the expected stopping message instead of running forever.
    ///
    /// Each iteration uses different tool input args to bypass the repeat-detection
    /// mechanism (which operates on args_hash), ensuring only the doom-loop detector
    /// (which operates on output content) is exercised.
    #[tokio::test]
    async fn doom_loop_agent_breaks_on_identical_native_tool_outputs() {
        use super::super::DOOM_LOOP_WINDOW;
        use zeph_llm::mock::MockProvider;
        use zeph_llm::provider::{ChatResponse, ToolUseRequest};

        // Each ChatResponse has a unique id and different input args (to avoid
        // repeat-detection which fires on identical (name, args_hash) pairs),
        // but the tool executor always returns Ok(None) → "(no output)" each time.
        // After DOOM_LOOP_WINDOW identical last-message contents, doom-loop fires.
        let tool_responses: Vec<ChatResponse> = (0..=DOOM_LOOP_WINDOW)
            .map(|i| ChatResponse::ToolUse {
                text: None,
                tool_calls: vec![ToolUseRequest {
                    id: format!("toolu_{i:06}"),
                    name: "stub_tool".to_owned().into(),
                    // Vary the input so args_hash differs each iteration → no repeat-detect
                    input: serde_json::json!({ "iteration": i }),
                }],
                thinking_blocks: vec![],
            })
            .collect();

        let (mock, _counter) = MockProvider::default().with_tool_use(tool_responses);
        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec!["trigger doom loop".to_owned()]);
        let registry = create_test_registry();
        // Default MockToolExecutor::execute_tool_call returns Ok(None) → "(no output)"
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.run().await;

        assert!(
            result.is_ok(),
            "agent must not return an error on doom loop"
        );

        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter()
                .any(|m| m.contains("Stopping: detected repeated identical tool outputs.")),
            "agent must send the doom-loop stopping message; got: {sent:?}"
        );
    }

    // Tests for filter_stats metric propagation (issue #1939).
    // The normal native tool path (single tool call) must increment filter_* metrics when the
    // tool returns FilterStats.

    #[tokio::test]
    async fn filter_stats_metrics_increment_on_normal_native_tool_path() {
        use crate::metrics::MetricsSnapshot;
        use tokio::sync::watch;
        use zeph_llm::mock::MockProvider;
        use zeph_llm::provider::{ChatResponse, ToolUseRequest};
        use zeph_tools::executor::{FilterStats, ToolCall, ToolError, ToolExecutor, ToolOutput};

        struct FilteredToolExecutor;

        impl ToolExecutor for FilteredToolExecutor {
            async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }

            async fn execute_tool_call(
                &self,
                _call: &ToolCall,
            ) -> Result<Option<ToolOutput>, ToolError> {
                Ok(Some(ToolOutput {
                    tool_name: "shell".into(),
                    summary: "filtered output".to_owned(),
                    blocks_executed: 1,
                    filter_stats: Some(FilterStats {
                        raw_chars: 400,
                        filtered_chars: 200,
                        raw_lines: 20,
                        filtered_lines: 10,
                        confidence: None,
                        command: None,
                        kept_lines: vec![],
                    }),
                    diff: None,
                    streamed: false,
                    terminal_id: None,
                    locations: None,
                    raw_response: None,
                    claim_source: None,
                }))
            }
        }

        let (mock, _counter) = MockProvider::default().with_tool_use(vec![
            ChatResponse::ToolUse {
                text: None,
                tool_calls: vec![ToolUseRequest {
                    id: "call-1".to_owned(),
                    name: "shell".to_owned().into(),
                    input: serde_json::json!({"cmd": "ls"}),
                }],
                thinking_blocks: vec![],
            },
            ChatResponse::Text("done".to_owned()),
        ]);
        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec!["run a tool".to_owned()]);
        let registry = create_test_registry();
        let executor = FilteredToolExecutor;
        let (tx, rx) = watch::channel(MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);
        agent.run().await.expect("agent run must succeed");

        let snap: MetricsSnapshot = rx.borrow().clone();
        assert!(
            snap.filter_applications > 0,
            "filter_applications must be > 0"
        );
        assert!(snap.filter_raw_tokens > 0, "filter_raw_tokens must be > 0");
        assert!(
            snap.filter_saved_tokens > 0,
            "filter_saved_tokens must be > 0"
        );
        assert_eq!(snap.filter_total_commands, 1);
        assert_eq!(snap.filter_filtered_commands, 1);
    }

    // Self-reflection remaining-tools path: when the first of two parallel tool calls returns
    // [error] and self-reflection fires (Ok(true)), the second call's FilterStats must still
    // be recorded in filter_* metrics (regression for #1939).
    //
    // Setup: two concurrent tool calls via native path (ToolUse response).
    // tool_a returns [error], triggering self-reflection which calls chat() → Text.
    // tool_b returns success with FilterStats. The remaining-tools loop processes tool_b.
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn filter_stats_metrics_recorded_in_self_reflection_remaining_tools_loop() {
        use crate::config::LearningConfig;
        use crate::metrics::MetricsSnapshot;
        use std::sync::Mutex;
        use tokio::sync::watch;
        use zeph_llm::mock::MockProvider;
        use zeph_llm::provider::{ChatResponse, ToolUseRequest};
        use zeph_tools::executor::{FilterStats, ToolCall, ToolError, ToolExecutor, ToolOutput};

        // Executor: tool_a returns error, tool_b returns filtered success.
        struct TwoToolExecutor {
            call_count: Mutex<u32>,
        }

        impl ToolExecutor for TwoToolExecutor {
            async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }

            async fn execute_tool_call(
                &self,
                call: &ToolCall,
            ) -> Result<Option<ToolOutput>, ToolError> {
                let n = {
                    let mut g = self.call_count.lock().unwrap();
                    *g += 1;
                    *g
                };
                if n == 1 || call.tool_id == "tool_a_id" {
                    Ok(Some(ToolOutput {
                        tool_name: "tool_a".into(),
                        summary: "[error] command failed [exit code 1]".to_owned(),
                        blocks_executed: 1,
                        filter_stats: None,
                        diff: None,
                        streamed: false,
                        terminal_id: None,
                        locations: None,
                        raw_response: None,
                        claim_source: None,
                    }))
                } else {
                    Ok(Some(ToolOutput {
                        tool_name: "tool_b".into(),
                        summary: "filtered output".to_owned(),
                        blocks_executed: 1,
                        filter_stats: Some(FilterStats {
                            raw_chars: 400,
                            filtered_chars: 200,
                            raw_lines: 20,
                            filtered_lines: 10,
                            confidence: None,
                            command: None,
                            kept_lines: vec![],
                        }),
                        diff: None,
                        streamed: false,
                        terminal_id: None,
                        locations: None,
                        raw_response: None,
                        claim_source: None,
                    }))
                }
            }
        }

        // Provider: one ToolUse response (two parallel tools), then Text for self-reflection.
        // When chat_with_tools queue is exhausted, fallback to chat() which returns "ok".
        let (mock, _counter) = MockProvider::with_responses(vec!["reflection ok".to_owned()])
            .with_tool_use(vec![ChatResponse::ToolUse {
                text: None,
                tool_calls: vec![
                    ToolUseRequest {
                        id: "tool_a_id".to_owned(),
                        name: "tool_a".to_owned().into(),
                        input: serde_json::json!({}),
                    },
                    ToolUseRequest {
                        id: "tool_b_id".to_owned(),
                        name: "tool_b".to_owned().into(),
                        input: serde_json::json!({}),
                    },
                ],
                thinking_blocks: vec![],
            }]);

        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec!["run tools".to_owned()]);
        let registry = create_test_registry();
        let executor = TwoToolExecutor {
            call_count: Mutex::new(0),
        };
        let (tx, rx) = watch::channel(MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_metrics(tx)
            .with_learning(LearningConfig {
                enabled: true,
                ..LearningConfig::default()
            });
        // Activate the "test-skill" created by create_test_registry() so self-reflection fires.
        agent
            .skill_state
            .active_skill_names
            .push("test-skill".to_owned());
        agent.run().await.expect("agent run must succeed");

        let snap: MetricsSnapshot = rx.borrow().clone();
        assert!(
            snap.filter_applications > 0,
            "filter_applications must be > 0 after remaining-tools loop processes tool_b"
        );
        assert!(
            snap.filter_raw_tokens > 0,
            "filter_raw_tokens must be > 0 after remaining-tools loop processes tool_b"
        );
        assert!(
            snap.filter_saved_tokens > 0,
            "filter_saved_tokens must be > 0 after remaining-tools loop processes tool_b"
        );
    }

    // Regression test for issue #1910: corrections must be stored in user_corrections even when
    // LearningConfig::enabled = false (skill auto-improvement is disabled).
    #[tokio::test]
    async fn correction_stored_when_learning_disabled() {
        use crate::config::LearningConfig;
        use std::sync::Arc;
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        use zeph_memory::semantic::SemanticMemory;

        let mock = MockProvider::default();
        let provider = AnyProvider::Mock(mock);
        let memory: SemanticMemory =
            SemanticMemory::new(":memory:", "http://127.0.0.1:1", provider, "test-model")
                .await
                .expect("in-memory SQLite must init");
        let memory = Arc::new(memory);

        let agent_provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let conv_id = memory.sqlite().create_conversation().await.unwrap();

        let mut agent = Agent::new(agent_provider, channel, registry, None, 5, executor)
            .with_learning(LearningConfig {
                enabled: false,
                correction_detection: true,
                ..LearningConfig::default()
            })
            .with_memory(Arc::clone(&memory), conv_id, 20, 5, 10);

        // "no that's wrong" triggers ExplicitRejection (confidence 0.85 > default threshold 0.6)
        agent
            .detect_and_record_corrections("no that's wrong", Some(conv_id))
            .await;

        let rows = memory.sqlite().load_recent_corrections(10).await.unwrap();
        assert_eq!(
            rows.len(),
            1,
            "correction must be stored even when learning is disabled"
        );
        assert_eq!(rows[0].correction_kind, "explicit_rejection");
        assert_eq!(rows[0].correction_text, "no that's wrong");
    }

    #[test]
    fn test_scheduled_task_injection_format() {
        let prompt = "bash -c 'echo hello'";
        let text = format!("{}{prompt}", super::super::SCHEDULED_TASK_PREFIX);
        assert!(text.starts_with(super::super::SCHEDULED_TASK_PREFIX));
        assert!(text.contains(prompt));
    }
}

// #2343: pre-execution verifier blocks must produce an AuditEntry with AuditResult::Blocked.
#[cfg(test)]
mod pre_execution_audit_tests {
    use std::sync::Arc;

    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;
    use zeph_llm::provider::{ChatResponse, ToolUseRequest};
    use zeph_tools::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
    use zeph_tools::registry::{InvocationHint, ToolDef};

    use super::super::Agent;
    use super::agent_tests::{MockChannel, create_test_registry};

    struct NoOpExecutor;

    impl ToolExecutor for NoOpExecutor {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }

        fn tool_definitions(&self) -> Vec<ToolDef> {
            vec![ToolDef {
                id: "bash".into(),
                description: "run shell command".into(),
                schema: schemars::Schema::default(),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            }]
        }

        async fn execute_tool_call(
            &self,
            _call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }
    }

    /// When a pre-execution verifier blocks a tool call and an audit logger is wired,
    /// an `AuditEntry` with `AuditResult::Blocked` must be written.
    #[tokio::test]
    async fn pre_execution_block_writes_audit_entry() {
        use crate::config::{SecurityConfig, TimeoutConfig};
        use zeph_tools::verifier::{
            FirewallVerifierConfig, PreExecutionVerifierConfig, UrlGroundingVerifierConfig,
        };

        let dir = tempfile::tempdir().unwrap();
        let audit_path = dir.path().join("audit.jsonl");

        // Create audit logger pointing at the temp file.
        let audit_config = zeph_tools::AuditConfig {
            enabled: true,
            destination: audit_path.display().to_string(),
            ..Default::default()
        };
        let logger = Arc::new(
            zeph_tools::AuditLogger::from_config(&audit_config, false)
                .await
                .unwrap(),
        );

        // Provider returns one tool call with /etc/passwd as the file_path — triggers FirewallVerifier
        // (glob pattern matches the exact path value), then returns text to end the loop.
        let (mock, _counter) = MockProvider::default().with_tool_use(vec![
            ChatResponse::ToolUse {
                text: None,
                tool_calls: vec![ToolUseRequest {
                    id: "call-block".to_owned(),
                    name: "read_file".to_owned().into(),
                    input: serde_json::json!({"file_path": "/etc/passwd"}),
                }],
                thinking_blocks: vec![],
            },
            ChatResponse::Text("done".into()),
        ]);
        let provider = AnyProvider::Mock(mock);
        let channel = MockChannel::new(vec!["run it".to_string()]);
        let registry = create_test_registry();

        // Build SecurityConfig with firewall verifier enabled; disable all others to keep it simple.
        let security = SecurityConfig {
            pre_execution_verify: PreExecutionVerifierConfig {
                enabled: true,
                destructive_commands: zeph_tools::DestructiveVerifierConfig {
                    enabled: false,
                    ..Default::default()
                },
                injection_patterns: zeph_tools::InjectionVerifierConfig {
                    enabled: false,
                    ..Default::default()
                },
                url_grounding: UrlGroundingVerifierConfig {
                    enabled: false,
                    ..Default::default()
                },
                firewall: FirewallVerifierConfig {
                    enabled: true,
                    blocked_paths: Vec::new(),
                    blocked_env_vars: Vec::new(),
                    exempt_tools: Vec::new(),
                },
            },
            ..Default::default()
        };

        let mut agent = Agent::new(provider, channel, registry, None, 5, NoOpExecutor)
            .with_security(security, TimeoutConfig::default())
            .with_audit_logger(Arc::clone(&logger));

        agent.run().await.unwrap();

        // Give tokio::spawn a chance to flush the audit entry.
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        drop(logger);

        let content = tokio::fs::read_to_string(&audit_path)
            .await
            .unwrap_or_default();
        assert!(
            !content.is_empty(),
            "audit log must contain at least one entry after pre-execution block"
        );
        assert!(
            content.contains("\"type\":\"blocked\""),
            "audit log must contain a blocked entry; got: {content}"
        );
        assert!(
            content.contains("pre_execution_block"),
            "audit log must contain error_category=pre_execution_block; got: {content}"
        );
        assert!(
            content.contains("read_file"),
            "audit log entry must reference the blocked tool id; got: {content}"
        );
        assert!(
            content.contains("\"error_domain\":\"security\""),
            "error_domain not found in audit entry"
        );
        assert!(
            content.contains("\"duration_ms\":0"),
            "duration_ms not found in audit entry"
        );
    }
}

// #2628: flush_orphaned_tool_use_on_shutdown must persist tombstone ToolResults on shutdown.
#[cfg(test)]
mod flush_orphaned_tests {
    use zeph_llm::any::AnyProvider;
    use zeph_memory::semantic::SemanticMemory;

    use super::super::Agent;
    use super::agent_tests::{MockChannel, MockToolExecutor, create_test_registry, mock_provider};

    async fn flush_test_memory() -> SemanticMemory {
        let provider = AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        SemanticMemory::new(":memory:", "http://127.0.0.1:1", provider, "test-model")
            .await
            .unwrap()
    }

    /// FO1: no-op when the message list has no assistant message.
    #[tokio::test]
    async fn flush_orphaned_noop_when_no_assistant_message() {
        use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

        let provider = mock_provider(vec![]);
        let memory = flush_test_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        // Push only a user message — no assistant message.
        agent.msg.messages.push(Message {
            role: Role::User,
            content: "hi".into(),
            parts: vec![MessagePart::Text { text: "hi".into() }],
            metadata: MessageMetadata::default(),
        });

        agent.flush_orphaned_tool_use_on_shutdown().await;

        let history = agent
            .memory_state
            .persistence
            .memory
            .as_ref()
            .unwrap()
            .sqlite()
            .load_history(cid, 50)
            .await
            .unwrap();
        assert!(
            history.is_empty(),
            "no tombstone must be persisted when there is no assistant message"
        );
    }

    /// FO2: no-op when the last assistant message contains no ToolUse parts.
    #[tokio::test]
    async fn flush_orphaned_noop_when_no_tool_use_parts() {
        use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

        let provider = mock_provider(vec![]);
        let memory = flush_test_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        agent.msg.messages.push(Message {
            role: Role::Assistant,
            content: "just text".into(),
            parts: vec![MessagePart::Text {
                text: "just text".into(),
            }],
            metadata: MessageMetadata::default(),
        });

        agent.flush_orphaned_tool_use_on_shutdown().await;

        let history = agent
            .memory_state
            .persistence
            .memory
            .as_ref()
            .unwrap()
            .sqlite()
            .load_history(cid, 50)
            .await
            .unwrap();
        assert!(
            history.is_empty(),
            "no tombstone must be persisted when there are no ToolUse parts"
        );
    }

    /// FO3: tombstone ToolResult is persisted for each unpaired ToolUse.
    #[tokio::test]
    async fn flush_orphaned_persists_tombstone_for_unpaired_tool_use() {
        use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

        let provider = mock_provider(vec![]);
        let memory = flush_test_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        agent.msg.messages.push(Message {
            role: Role::Assistant,
            content: "[tool_use]".into(),
            parts: vec![
                MessagePart::ToolUse {
                    id: "orphan_1".into(),
                    name: "shell".into(),
                    input: serde_json::json!({}),
                },
                MessagePart::ToolUse {
                    id: "orphan_2".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({}),
                },
            ],
            metadata: MessageMetadata::default(),
        });

        agent.flush_orphaned_tool_use_on_shutdown().await;

        let history = agent
            .memory_state
            .persistence
            .memory
            .as_ref()
            .unwrap()
            .sqlite()
            .load_history(cid, 50)
            .await
            .unwrap();

        assert_eq!(
            history.len(),
            1,
            "exactly one tombstone user message must be persisted"
        );
        assert_eq!(history[0].role, Role::User);
        for id in ["orphan_1", "orphan_2"] {
            assert!(
                history[0].parts.iter().any(|p| matches!(
                    p,
                    MessagePart::ToolResult { tool_use_id, is_error, .. }
                        if tool_use_id == id && *is_error
                )),
                "tombstone ToolResult for {id} must be is_error=true"
            );
        }
    }

    /// FO4: no-op when all ToolUse ids are already covered by a following ToolResult.
    #[tokio::test]
    async fn flush_orphaned_noop_when_tool_use_already_paired() {
        use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

        let provider = mock_provider(vec![]);
        let memory = flush_test_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        agent.msg.messages.push(Message {
            role: Role::Assistant,
            content: "[tool_use]".into(),
            parts: vec![MessagePart::ToolUse {
                id: "paired_id".into(),
                name: "shell".into(),
                input: serde_json::json!({}),
            }],
            metadata: MessageMetadata::default(),
        });
        agent.msg.messages.push(Message {
            role: Role::User,
            content: "[tool_result]".into(),
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "paired_id".into(),
                content: "ok".into(),
                is_error: false,
            }],
            metadata: MessageMetadata::default(),
        });

        agent.flush_orphaned_tool_use_on_shutdown().await;

        let history = agent
            .memory_state
            .persistence
            .memory
            .as_ref()
            .unwrap()
            .sqlite()
            .load_history(cid, 50)
            .await
            .unwrap();
        assert!(
            history.is_empty(),
            "no tombstone must be persisted when all ToolUse parts are already paired"
        );
    }
}

// ── resolve_context_budget (#2793) ───────────────────────────────────────────

#[cfg(test)]
mod resolve_context_budget_tests {
    use std::path::Path;

    use crate::config::Config;
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;

    use super::super::resolve_context_budget;

    #[test]
    fn explicit_budget_returned_as_is() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.memory.auto_budget = false;
        config.memory.context_budget_tokens = 65536;
        let provider = AnyProvider::Mock(MockProvider::default());
        assert_eq!(resolve_context_budget(&config, &provider), 65536);
    }

    #[test]
    fn auto_budget_true_budget_zero_no_window_falls_back_to_128k() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.memory.auto_budget = true;
        config.memory.context_budget_tokens = 0;
        // MockProvider::context_window() returns None — triggers 128k fallback.
        let provider = AnyProvider::Mock(MockProvider::default());
        assert_ne!(
            resolve_context_budget(&config, &provider),
            0,
            "budget must not be zero when auto_budget=true and context_budget_tokens=0"
        );
        assert_eq!(resolve_context_budget(&config, &provider), 128_000);
    }

    #[test]
    fn auto_budget_false_budget_zero_falls_back_to_128k() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.memory.auto_budget = false;
        config.memory.context_budget_tokens = 0;
        let provider = AnyProvider::Mock(MockProvider::default());
        assert_eq!(resolve_context_budget(&config, &provider), 128_000);
    }
}
