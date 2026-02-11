use tokio::sync::mpsc;
use zeph_core::channel::{Channel, ChannelMessage};

use crate::event::AgentEvent;

#[derive(Debug)]
pub struct TuiChannel {
    user_input_rx: mpsc::Receiver<String>,
    agent_event_tx: mpsc::Sender<AgentEvent>,
    accumulated: String,
}

impl TuiChannel {
    #[must_use]
    pub fn new(
        user_input_rx: mpsc::Receiver<String>,
        agent_event_tx: mpsc::Sender<AgentEvent>,
    ) -> Self {
        Self {
            user_input_rx,
            agent_event_tx,
            accumulated: String::new(),
        }
    }
}

impl Channel for TuiChannel {
    async fn recv(&mut self) -> anyhow::Result<Option<ChannelMessage>> {
        match self.user_input_rx.recv().await {
            Some(text) => {
                self.accumulated.clear();
                Ok(Some(ChannelMessage { text }))
            }
            None => Ok(None),
        }
    }

    async fn send(&mut self, text: &str) -> anyhow::Result<()> {
        self.agent_event_tx
            .send(AgentEvent::FullMessage(text.to_owned()))
            .await
            .map_err(|_| anyhow::anyhow!("TUI channel closed"))
    }

    async fn send_chunk(&mut self, chunk: &str) -> anyhow::Result<()> {
        self.accumulated.push_str(chunk);
        self.agent_event_tx
            .send(AgentEvent::Chunk(chunk.to_owned()))
            .await
            .map_err(|_| anyhow::anyhow!("TUI channel closed"))
    }

    async fn flush_chunks(&mut self) -> anyhow::Result<()> {
        self.agent_event_tx
            .send(AgentEvent::Flush)
            .await
            .map_err(|_| anyhow::anyhow!("TUI channel closed"))
    }

    async fn send_typing(&mut self) -> anyhow::Result<()> {
        self.agent_event_tx
            .send(AgentEvent::Typing)
            .await
            .map_err(|_| anyhow::anyhow!("TUI channel closed"))
    }

    async fn confirm(&mut self, prompt: &str) -> anyhow::Result<bool> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.agent_event_tx
            .send(AgentEvent::ConfirmRequest {
                prompt: prompt.to_owned(),
                response_tx: tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("TUI channel closed"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("confirm dialog cancelled"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_channel() -> (TuiChannel, mpsc::Sender<String>, mpsc::Receiver<AgentEvent>) {
        let (user_tx, user_rx) = mpsc::channel(16);
        let (agent_tx, agent_rx) = mpsc::channel(16);
        let channel = TuiChannel::new(user_rx, agent_tx);
        (channel, user_tx, agent_rx)
    }

    #[tokio::test]
    async fn recv_returns_user_input() {
        let (mut ch, user_tx, _agent_rx) = make_channel();
        user_tx.send("hello".into()).await.unwrap();
        let msg = ch.recv().await.unwrap().unwrap();
        assert_eq!(msg.text, "hello");
    }

    #[tokio::test]
    async fn recv_returns_none_when_sender_dropped() {
        let (mut ch, user_tx, _agent_rx) = make_channel();
        drop(user_tx);
        let msg = ch.recv().await.unwrap();
        assert!(msg.is_none());
    }

    #[tokio::test]
    async fn send_forwards_full_message() {
        let (mut ch, _user_tx, mut agent_rx) = make_channel();
        ch.send("response text").await.unwrap();
        let evt = agent_rx.recv().await.unwrap();
        assert!(matches!(evt, AgentEvent::FullMessage(t) if t == "response text"));
    }

    #[tokio::test]
    async fn send_chunk_forwards_and_accumulates() {
        let (mut ch, _user_tx, mut agent_rx) = make_channel();
        ch.send_chunk("hel").await.unwrap();
        ch.send_chunk("lo").await.unwrap();
        assert_eq!(ch.accumulated, "hello");

        let e1 = agent_rx.recv().await.unwrap();
        assert!(matches!(e1, AgentEvent::Chunk(t) if t == "hel"));
        let e2 = agent_rx.recv().await.unwrap();
        assert!(matches!(e2, AgentEvent::Chunk(t) if t == "lo"));
    }

    #[tokio::test]
    async fn flush_chunks_sends_flush_event() {
        let (mut ch, _user_tx, mut agent_rx) = make_channel();
        ch.flush_chunks().await.unwrap();
        let evt = agent_rx.recv().await.unwrap();
        assert!(matches!(evt, AgentEvent::Flush));
    }

    #[tokio::test]
    async fn send_typing_sends_typing_event() {
        let (mut ch, _user_tx, mut agent_rx) = make_channel();
        ch.send_typing().await.unwrap();
        let evt = agent_rx.recv().await.unwrap();
        assert!(matches!(evt, AgentEvent::Typing));
    }

    #[tokio::test]
    async fn confirm_sends_request_and_returns_response() {
        let (mut ch, _user_tx, mut agent_rx) = make_channel();

        let confirm_fut = tokio::spawn(async move { ch.confirm("delete?").await.unwrap() });

        let evt = agent_rx.recv().await.unwrap();
        if let AgentEvent::ConfirmRequest {
            prompt,
            response_tx,
        } = evt
        {
            assert_eq!(prompt, "delete?");
            response_tx.send(true).unwrap();
        } else {
            panic!("expected ConfirmRequest");
        }

        assert!(confirm_fut.await.unwrap());
    }

    #[tokio::test]
    async fn confirm_returns_false_on_rejection() {
        let (mut ch, _user_tx, mut agent_rx) = make_channel();

        let confirm_fut = tokio::spawn(async move { ch.confirm("proceed?").await.unwrap() });

        let evt = agent_rx.recv().await.unwrap();
        if let AgentEvent::ConfirmRequest { response_tx, .. } = evt {
            response_tx.send(false).unwrap();
        } else {
            panic!("expected ConfirmRequest");
        }

        assert!(!confirm_fut.await.unwrap());
    }

    #[tokio::test]
    async fn confirm_errors_when_receiver_dropped() {
        let (mut ch, _user_tx, mut agent_rx) = make_channel();

        let confirm_fut = tokio::spawn(async move { ch.confirm("test?").await });

        let evt = agent_rx.recv().await.unwrap();
        if let AgentEvent::ConfirmRequest { response_tx, .. } = evt {
            drop(response_tx);
        }

        assert!(confirm_fut.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn recv_clears_accumulated() {
        let (mut ch, user_tx, _agent_rx) = make_channel();
        ch.accumulated = "old data".into();
        user_tx.send("new".into()).await.unwrap();
        ch.recv().await.unwrap();
        assert!(ch.accumulated.is_empty());
    }

    #[test]
    fn tui_channel_debug() {
        let (ch, _user_tx, _agent_rx) = make_channel();
        let debug = format!("{ch:?}");
        assert!(debug.contains("TuiChannel"));
    }
}
