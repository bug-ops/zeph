// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[allow(unused_imports)]
use super::*;

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
        ) -> Pin<Box<dyn Future<Output = Result<Transcription, LlmError>> + Send + '_>> {
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
