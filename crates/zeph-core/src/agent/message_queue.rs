use std::time::{Duration, Instant};

use crate::channel::Channel;

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

impl<C: Channel> Agent<C> {
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
}
