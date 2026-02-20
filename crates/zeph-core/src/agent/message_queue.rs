use std::time::{Duration, Instant};

use crate::channel::Channel;
use zeph_tools::executor::ToolExecutor;

use super::Agent;

pub(super) const MAX_QUEUE_SIZE: usize = 10;
pub(super) const MESSAGE_MERGE_WINDOW: Duration = Duration::from_millis(500);
pub(super) const MAX_AUDIO_BYTES: usize = 25 * 1024 * 1024;
pub(super) const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

pub(super) struct QueuedMessage {
    pub(super) text: String,
    pub(super) received_at: Instant,
    pub(super) image_parts: Vec<zeph_llm::provider::MessagePart>,
    pub(super) raw_attachments: Vec<crate::channel::Attachment>,
}

pub(super) fn detect_image_mime(filename: Option<&str>) -> &'static str {
    let ext = filename
        .and_then(|f| std::path::Path::new(f).extension())
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if ext.eq_ignore_ascii_case("jpg") || ext.eq_ignore_ascii_case("jpeg") {
        "image/jpeg"
    } else if ext.eq_ignore_ascii_case("gif") {
        "image/gif"
    } else if ext.eq_ignore_ascii_case("webp") {
        "image/webp"
    } else {
        "image/png"
    }
}

impl<C: Channel, T: ToolExecutor> Agent<C, T> {
    pub(super) fn drain_channel(&mut self) {
        while self.message_queue.len() < MAX_QUEUE_SIZE {
            let Some(msg) = self.channel.try_recv() else {
                break;
            };
            if msg.text.trim() == "/drop-last-queued" {
                self.message_queue.pop_back();
                continue;
            }
            self.enqueue_or_merge(msg.text, vec![], msg.attachments);
        }
    }

    pub(super) fn enqueue_or_merge(
        &mut self,
        text: String,
        image_parts: Vec<zeph_llm::provider::MessagePart>,
        raw_attachments: Vec<crate::channel::Attachment>,
    ) {
        let now = Instant::now();
        if let Some(last) = self.message_queue.back_mut()
            && now.duration_since(last.received_at) < MESSAGE_MERGE_WINDOW
            && last.image_parts.is_empty()
            && image_parts.is_empty()
            && last.raw_attachments.is_empty()
            && raw_attachments.is_empty()
        {
            last.text.push('\n');
            last.text.push_str(&text);
            return;
        }
        if self.message_queue.len() < MAX_QUEUE_SIZE {
            self.message_queue.push_back(QueuedMessage {
                text,
                received_at: now,
                image_parts,
                raw_attachments,
            });
        } else {
            tracing::warn!("message queue full, dropping message");
        }
    }

    pub(super) async fn notify_queue_count(&mut self) {
        let count = self.message_queue.len();
        let _ = self.channel.send_queue_count(count).await;
    }

    pub(super) fn clear_queue(&mut self) -> usize {
        let count = self.message_queue.len();
        self.message_queue.clear();
        count
    }

    pub(super) async fn resolve_message(
        &self,
        msg: crate::channel::ChannelMessage,
    ) -> (String, Vec<zeph_llm::provider::MessagePart>) {
        use crate::channel::{Attachment, AttachmentKind};
        use zeph_llm::provider::MessagePart;

        let text_base = msg.text.clone();

        let (audio_attachments, image_attachments): (Vec<Attachment>, Vec<Attachment>) = msg
            .attachments
            .into_iter()
            .partition(|a| a.kind == AttachmentKind::Audio);

        tracing::debug!(
            audio = audio_attachments.len(),
            has_stt = self.stt.is_some(),
            "resolve_message attachments"
        );

        let text = if !audio_attachments.is_empty()
            && let Some(stt) = self.stt.as_ref()
        {
            let mut transcribed_parts = Vec::new();
            for attachment in &audio_attachments {
                if attachment.data.len() > MAX_AUDIO_BYTES {
                    tracing::warn!(
                        size = attachment.data.len(),
                        max = MAX_AUDIO_BYTES,
                        "audio attachment exceeds size limit, skipping"
                    );
                    continue;
                }
                match stt
                    .transcribe(&attachment.data, attachment.filename.as_deref())
                    .await
                {
                    Ok(result) => {
                        tracing::info!(
                            len = result.text.len(),
                            language = ?result.language,
                            "audio transcribed"
                        );
                        transcribed_parts.push(result.text);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "audio transcription failed");
                    }
                }
            }
            if transcribed_parts.is_empty() {
                text_base
            } else {
                let transcribed = transcribed_parts.join("\n");
                if text_base.is_empty() {
                    transcribed
                } else {
                    format!("[transcribed audio]\n{transcribed}\n\n{text_base}")
                }
            }
        } else {
            if !audio_attachments.is_empty() {
                tracing::warn!(
                    count = audio_attachments.len(),
                    "audio attachments received but no STT provider configured, dropping"
                );
            }
            text_base
        };

        let mut image_parts = Vec::new();
        for attachment in image_attachments {
            if attachment.data.len() > MAX_IMAGE_BYTES {
                tracing::warn!(
                    size = attachment.data.len(),
                    max = MAX_IMAGE_BYTES,
                    "image attachment exceeds size limit, skipping"
                );
                continue;
            }
            let mime_type = detect_image_mime(attachment.filename.as_deref()).to_string();
            image_parts.push(MessagePart::Image {
                data: attachment.data,
                mime_type,
            });
        }

        (text, image_parts)
    }

    pub(super) async fn handle_image_command(
        &mut self,
        path: &str,
        extra_parts: &mut Vec<zeph_llm::provider::MessagePart>,
    ) -> Result<(), super::error::AgentError> {
        use std::path::Component;
        use zeph_llm::provider::MessagePart;

        // Reject paths that traverse outside the current directory.
        let has_parent_dir = std::path::Path::new(path)
            .components()
            .any(|c| c == Component::ParentDir);
        if has_parent_dir {
            self.channel
                .send("Invalid image path: path traversal not allowed")
                .await?;
            return Ok(());
        }

        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                self.channel
                    .send(&format!("Cannot read image {path}: {e}"))
                    .await?;
                return Ok(());
            }
        };
        if data.len() > MAX_IMAGE_BYTES {
            self.channel
                .send(&format!(
                    "Image {path} exceeds size limit ({} MB), skipping",
                    MAX_IMAGE_BYTES / 1024 / 1024
                ))
                .await?;
            return Ok(());
        }
        let mime_type = detect_image_mime(Some(path)).to_string();
        extra_parts.push(MessagePart::Image { data, mime_type });
        self.channel
            .send(&format!("Image loaded: {path}. Send your message."))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use super::*;

    #[test]
    fn enqueue_or_merge_adds_new_message() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.enqueue_or_merge("hello".into(), vec![], vec![]);
        assert_eq!(agent.message_queue.len(), 1);
        assert_eq!(agent.message_queue[0].text, "hello");
    }

    #[test]
    fn enqueue_or_merge_merges_within_window() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.enqueue_or_merge("first".into(), vec![], vec![]);
        agent.enqueue_or_merge("second".into(), vec![], vec![]);
        assert_eq!(agent.message_queue.len(), 1);
        assert_eq!(agent.message_queue[0].text, "first\nsecond");
    }

    #[test]
    fn enqueue_or_merge_no_merge_after_window() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.message_queue.push_back(QueuedMessage {
            text: "old".into(),
            received_at: Instant::now() - Duration::from_secs(2),
            image_parts: vec![],
            raw_attachments: vec![],
        });
        agent.enqueue_or_merge("new".into(), vec![], vec![]);
        assert_eq!(agent.message_queue.len(), 2);
        assert_eq!(agent.message_queue[0].text, "old");
        assert_eq!(agent.message_queue[1].text, "new");
    }

    #[test]
    fn enqueue_or_merge_respects_max_queue_size() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        for i in 0..MAX_QUEUE_SIZE {
            agent.message_queue.push_back(QueuedMessage {
                text: format!("msg{i}"),
                received_at: Instant::now() - Duration::from_secs(2),
                image_parts: vec![],
                raw_attachments: vec![],
            });
        }
        agent.enqueue_or_merge("overflow".into(), vec![], vec![]);
        assert_eq!(agent.message_queue.len(), MAX_QUEUE_SIZE);
    }

    #[test]
    fn clear_queue_returns_count_and_empties() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.enqueue_or_merge("a".into(), vec![], vec![]);
        // Wait past merge window
        agent.message_queue.back_mut().unwrap().received_at =
            Instant::now() - Duration::from_secs(1);
        agent.enqueue_or_merge("b".into(), vec![], vec![]);
        assert_eq!(agent.message_queue.len(), 2);

        let count = agent.clear_queue();
        assert_eq!(count, 2);
        assert!(agent.message_queue.is_empty());
    }

    #[test]
    fn drain_channel_fills_queue() {
        let messages: Vec<String> = (0..5).map(|i| format!("msg{i}")).collect();
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(messages);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.drain_channel();
        // All 5 messages arrive within the merge window, so they merge into 1
        assert_eq!(agent.message_queue.len(), 1);
        assert!(agent.message_queue[0].text.contains("msg0"));
        assert!(agent.message_queue[0].text.contains("msg4"));
    }

    #[test]
    fn drain_channel_stops_at_max_queue_size() {
        let messages: Vec<String> = (0..15).map(|i| format!("msg{i}")).collect();
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(messages);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        // Pre-fill queue to near capacity with old timestamps (outside merge window)
        for i in 0..MAX_QUEUE_SIZE - 1 {
            agent.message_queue.push_back(QueuedMessage {
                text: format!("pre{i}"),
                received_at: Instant::now() - Duration::from_secs(2),
                image_parts: vec![],
                raw_attachments: vec![],
            });
        }
        agent.drain_channel();
        // One more slot was available; all 15 messages merge into it
        assert_eq!(agent.message_queue.len(), MAX_QUEUE_SIZE);
    }

    #[test]
    fn queue_fifo_order() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        for i in 0..3 {
            agent.message_queue.push_back(QueuedMessage {
                text: format!("msg{i}"),
                received_at: Instant::now() - Duration::from_secs(2),
                image_parts: vec![],
                raw_attachments: vec![],
            });
        }

        assert_eq!(agent.message_queue.pop_front().unwrap().text, "msg0");
        assert_eq!(agent.message_queue.pop_front().unwrap().text, "msg1");
        assert_eq!(agent.message_queue.pop_front().unwrap().text, "msg2");
    }

    #[test]
    fn detect_image_mime_standard() {
        assert_eq!(detect_image_mime(Some("photo.jpg")), "image/jpeg");
        assert_eq!(detect_image_mime(Some("photo.jpeg")), "image/jpeg");
        assert_eq!(detect_image_mime(Some("anim.gif")), "image/gif");
        assert_eq!(detect_image_mime(Some("img.webp")), "image/webp");
        assert_eq!(detect_image_mime(Some("img.png")), "image/png");
        assert_eq!(detect_image_mime(None), "image/png");
    }

    #[test]
    fn detect_image_mime_uppercase() {
        assert_eq!(detect_image_mime(Some("photo.JPG")), "image/jpeg");
        assert_eq!(detect_image_mime(Some("photo.JPEG")), "image/jpeg");
        assert_eq!(detect_image_mime(Some("anim.GIF")), "image/gif");
        assert_eq!(detect_image_mime(Some("img.WEBP")), "image/webp");
    }

    #[test]
    fn detect_image_mime_mixed_case() {
        assert_eq!(detect_image_mime(Some("photo.Jpg")), "image/jpeg");
        assert_eq!(detect_image_mime(Some("photo.JpEg")), "image/jpeg");
        assert_eq!(detect_image_mime(Some("anim.Gif")), "image/gif");
        assert_eq!(detect_image_mime(Some("img.WebP")), "image/webp");
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
        use crate::channel::{Attachment, AttachmentKind, ChannelMessage};
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
            zeph_llm::provider::MessagePart::Image { mime_type, data } => {
                assert_eq!(mime_type, "image/jpeg");
                assert_eq!(data.len(), 16);
            }
            _ => panic!("expected Image part"),
        }
    }

    #[tokio::test]
    async fn resolve_message_drops_oversized_image() {
        use crate::channel::{Attachment, AttachmentKind, ChannelMessage};
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

    #[tokio::test]
    async fn handle_image_command_rejects_path_traversal() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let mut parts = Vec::new();
        let result = agent
            .handle_image_command("../../etc/passwd", &mut parts)
            .await;
        assert!(result.is_ok());
        assert!(parts.is_empty());
        let sent = agent.channel.sent_messages();
        assert!(sent.iter().any(|m| m.contains("traversal")));
    }

    #[tokio::test]
    async fn handle_image_command_missing_file_sends_error() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let mut parts = Vec::new();
        let result = agent
            .handle_image_command("/nonexistent/image.png", &mut parts)
            .await;
        assert!(result.is_ok());
        assert!(parts.is_empty());
        let sent = agent.channel.sent_messages();
        assert!(sent.iter().any(|m| m.contains("Cannot read image")));
    }

    #[tokio::test]
    async fn handle_image_command_loads_valid_file() {
        use std::io::Write;
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let mut tmp = tempfile::NamedTempFile::with_suffix(".jpg").unwrap();
        let data = vec![0xFFu8, 0xD8, 0xFF, 0xE0];
        tmp.write_all(&data).unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();

        let mut parts = Vec::new();
        let result = agent.handle_image_command(&path, &mut parts).await;
        assert!(result.is_ok());
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            zeph_llm::provider::MessagePart::Image {
                data: img_data,
                mime_type,
            } => {
                assert_eq!(img_data, &data);
                assert_eq!(mime_type, "image/jpeg");
            }
            _ => panic!("expected Image part"),
        }
        let sent = agent.channel.sent_messages();
        assert!(sent.iter().any(|m| m.contains("Image loaded")));
    }

    mod resolve_message_tests {
        use super::super::super::agent_tests::{MockChannel, MockToolExecutor, mock_provider};
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

        fn make_agent(stt: Option<Box<dyn SpeechToText>>) -> Agent<MockChannel, MockToolExecutor> {
            let provider = mock_provider(vec!["ok".into()]);
            let empty: Vec<String> = vec![];
            let registry = zeph_skills::registry::SkillRegistry::load(&empty);
            let channel = MockChannel::new(vec![]);
            let executor = MockToolExecutor::no_tools();
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            agent.stt = stt;
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
}
