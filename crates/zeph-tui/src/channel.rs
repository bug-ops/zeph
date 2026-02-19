use tokio::sync::mpsc;
use zeph_core::channel::{Channel, ChannelError, ChannelMessage};

use crate::command::TuiCommand;
use crate::event::AgentEvent;

#[derive(Debug)]
pub struct TuiChannel {
    user_input_rx: mpsc::Receiver<String>,
    agent_event_tx: mpsc::Sender<AgentEvent>,
    accumulated: String,
    command_rx: Option<mpsc::Receiver<TuiCommand>>,
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
            command_rx: None,
        }
    }

    #[must_use]
    pub fn with_command_rx(mut self, rx: mpsc::Receiver<TuiCommand>) -> Self {
        self.command_rx = Some(rx);
        self
    }

    pub fn try_recv_command(&mut self) -> Option<TuiCommand> {
        self.command_rx.as_mut()?.try_recv().ok()
    }
}

impl Channel for TuiChannel {
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        match self.user_input_rx.recv().await {
            Some(text) => {
                self.accumulated.clear();
                Ok(Some(ChannelMessage {
                    text,
                    attachments: vec![],
                }))
            }
            None => Ok(None),
        }
    }

    fn try_recv(&mut self) -> Option<ChannelMessage> {
        self.user_input_rx.try_recv().ok().map(|text| {
            self.accumulated.clear();
            ChannelMessage {
                text,
                attachments: vec![],
            }
        })
    }

    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        self.agent_event_tx
            .send(AgentEvent::FullMessage(text.to_owned()))
            .await
            .map_err(|_| ChannelError::ChannelClosed)?;
        Ok(())
    }

    async fn send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        self.accumulated.push_str(chunk);
        self.agent_event_tx
            .send(AgentEvent::Chunk(chunk.to_owned()))
            .await
            .map_err(|_| ChannelError::ChannelClosed)?;
        Ok(())
    }

    async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
        self.agent_event_tx
            .send(AgentEvent::Flush)
            .await
            .map_err(|_| ChannelError::ChannelClosed)?;
        Ok(())
    }

    async fn send_typing(&mut self) -> Result<(), ChannelError> {
        self.agent_event_tx
            .send(AgentEvent::Typing)
            .await
            .map_err(|_| ChannelError::ChannelClosed)?;
        Ok(())
    }

    async fn send_status(&mut self, text: &str) -> Result<(), ChannelError> {
        self.agent_event_tx
            .send(AgentEvent::Status(text.to_owned()))
            .await
            .map_err(|_| ChannelError::ChannelClosed)?;
        Ok(())
    }

    async fn send_queue_count(&mut self, count: usize) -> Result<(), ChannelError> {
        self.agent_event_tx
            .send(AgentEvent::QueueCount(count))
            .await
            .map_err(|_| ChannelError::ChannelClosed)?;
        Ok(())
    }

    async fn send_diff(&mut self, diff: zeph_core::DiffData) -> Result<(), ChannelError> {
        self.agent_event_tx
            .send(AgentEvent::DiffReady(diff))
            .await
            .map_err(|_| ChannelError::ChannelClosed)?;
        Ok(())
    }

    async fn send_tool_output(
        &mut self,
        tool_name: &str,
        display: &str,
        diff: Option<zeph_core::DiffData>,
        filter_stats: Option<String>,
    ) -> Result<(), ChannelError> {
        tracing::debug!(
            %tool_name,
            has_diff = diff.is_some(),
            "TuiChannel::send_tool_output called"
        );
        self.agent_event_tx
            .send(AgentEvent::ToolOutput {
                tool_name: tool_name.to_owned(),
                command: display.to_owned(),
                output: display.to_owned(),
                success: true,
                diff,
                filter_stats,
            })
            .await
            .map_err(|_| ChannelError::ChannelClosed)?;
        Ok(())
    }

    async fn confirm(&mut self, prompt: &str) -> Result<bool, ChannelError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.agent_event_tx
            .send(AgentEvent::ConfirmRequest {
                prompt: prompt.to_owned(),
                response_tx: tx,
            })
            .await
            .map_err(|_| ChannelError::ChannelClosed)?;
        rx.await.map_err(|_| ChannelError::ConfirmCancelled)
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

    #[tokio::test]
    async fn send_status_sends_status_event() {
        let (mut ch, _user_tx, mut agent_rx) = make_channel();
        ch.send_status("summarizing...").await.unwrap();
        let evt = agent_rx.recv().await.unwrap();
        assert!(matches!(evt, AgentEvent::Status(t) if t == "summarizing..."));
    }

    #[test]
    fn try_recv_returns_none_when_empty() {
        let (mut ch, _user_tx, _agent_rx) = make_channel();
        assert!(ch.try_recv().is_none());
    }

    #[test]
    fn try_recv_returns_message() {
        let (mut ch, user_tx, _agent_rx) = make_channel();
        user_tx.try_send("queued".into()).unwrap();
        let msg = ch.try_recv().unwrap();
        assert_eq!(msg.text, "queued");
        assert!(ch.accumulated.is_empty());
    }

    #[tokio::test]
    async fn send_queue_count_forwards_event() {
        let (mut ch, _user_tx, mut agent_rx) = make_channel();
        ch.send_queue_count(3).await.unwrap();
        let evt = agent_rx.recv().await.unwrap();
        assert!(matches!(evt, AgentEvent::QueueCount(3)));
    }

    #[test]
    fn tui_channel_debug() {
        let (ch, _user_tx, _agent_rx) = make_channel();
        let debug = format!("{ch:?}");
        assert!(debug.contains("TuiChannel"));
    }

    #[test]
    fn try_recv_command_returns_none_without_receiver() {
        let (mut ch, _user_tx, _agent_rx) = make_channel();
        assert!(ch.try_recv_command().is_none());
    }

    #[test]
    fn try_recv_command_returns_none_when_empty() {
        let (ch, _user_tx, _agent_rx) = make_channel();
        let (_cmd_tx, cmd_rx) = mpsc::channel(16);
        let mut ch = ch.with_command_rx(cmd_rx);
        assert!(ch.try_recv_command().is_none());
    }

    #[test]
    fn try_recv_command_returns_sent_command() {
        let (ch, _user_tx, _agent_rx) = make_channel();
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        cmd_tx.try_send(TuiCommand::SkillList).unwrap();
        let mut ch = ch.with_command_rx(cmd_rx);
        let cmd = ch.try_recv_command().expect("should receive command");
        assert_eq!(cmd, TuiCommand::SkillList);
        assert!(ch.try_recv_command().is_none(), "second call returns None");
    }

    #[tokio::test]
    async fn send_tool_output_bundles_diff_atomically() {
        let (mut ch, _user_tx, mut agent_rx) = make_channel();
        let diff = zeph_core::DiffData {
            file_path: "src/main.rs".into(),
            old_content: "old".into(),
            new_content: "new".into(),
        };
        ch.send_tool_output(
            "bash",
            "[tool output: bash]\n```\nok\n```",
            Some(diff),
            None,
        )
        .await
        .unwrap();

        let evt = agent_rx.recv().await.unwrap();
        assert!(
            matches!(evt, AgentEvent::ToolOutput { ref tool_name, ref diff, .. } if tool_name == "bash" && diff.is_some()),
            "expected ToolOutput with diff"
        );
    }

    #[tokio::test]
    async fn send_tool_output_without_diff_sends_tool_event() {
        let (mut ch, _user_tx, mut agent_rx) = make_channel();
        ch.send_tool_output("read", "[tool output: read]\n```\ncontent\n```", None, None)
            .await
            .unwrap();

        let evt = agent_rx.recv().await.unwrap();
        assert!(
            matches!(evt, AgentEvent::ToolOutput { ref tool_name, .. } if tool_name == "read"),
            "expected ToolOutput"
        );
    }
}
