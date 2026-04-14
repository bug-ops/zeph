// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use tokio::sync::mpsc;
use zeph_core::channel::{
    Channel, ChannelError, ChannelMessage, ElicitationRequest, ElicitationResponse,
    ToolOutputEvent, ToolStartEvent,
};

use crate::command::TuiCommand;
use crate::event::AgentEvent;

/// The [`zeph_core::channel::Channel`] implementation for the TUI.
///
/// `TuiChannel` bridges the agent loop and the TUI render loop:
///
/// - **User input** arrives from [`App`](crate::App) via `user_input_rx` and
///   is forwarded to the agent as [`zeph_core::channel::ChannelMessage`].
/// - **Agent output** (chunks, tool events, status, diffs, confirmations) is
///   forwarded to the TUI via `agent_event_tx` as [`crate::event::AgentEvent`]
///   variants.
/// - An optional `command_rx` receives [`TuiCommand`] control messages without
///   going through the LLM loop (e.g. slash-commands).
///
/// # Examples
///
/// ```rust
/// use tokio::sync::mpsc;
/// use zeph_tui::TuiChannel;
/// use zeph_tui::event::AgentEvent;
///
/// let (user_tx, user_rx) = mpsc::channel(16);
/// let (agent_tx, _agent_rx) = mpsc::channel(16);
/// let channel = TuiChannel::new(user_rx, agent_tx);
/// ```
#[derive(Debug)]
pub struct TuiChannel {
    user_input_rx: mpsc::Receiver<String>,
    agent_event_tx: mpsc::Sender<AgentEvent>,
    accumulated: String,
    command_rx: Option<mpsc::Receiver<TuiCommand>>,
}

impl TuiChannel {
    /// Create a new `TuiChannel` from the given channel endpoints.
    ///
    /// # Arguments
    ///
    /// * `user_input_rx` — receives UTF-8 strings typed by the user in the input box.
    /// * `agent_event_tx` — sends agent lifecycle events to the TUI render loop.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::TuiChannel;
    /// use zeph_tui::event::AgentEvent;
    ///
    /// let (user_tx, user_rx) = mpsc::channel(16);
    /// let (agent_tx, _agent_rx) = mpsc::channel(16);
    /// let channel = TuiChannel::new(user_rx, agent_tx);
    /// ```
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

    /// Attach an optional command receiver for slash-command dispatch.
    ///
    /// When set, the agent loop can call [`try_recv_command`](Self::try_recv_command)
    /// to drain pending [`TuiCommand`] values without going through the LLM.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::{TuiChannel, TuiCommand};
    /// use zeph_tui::event::AgentEvent;
    ///
    /// let (user_tx, user_rx) = mpsc::channel(16);
    /// let (agent_tx, _agent_rx) = mpsc::channel(16);
    /// let (_cmd_tx, cmd_rx) = mpsc::channel(8);
    /// let channel = TuiChannel::new(user_rx, agent_tx).with_command_rx(cmd_rx);
    /// ```
    #[must_use]
    pub fn with_command_rx(mut self, rx: mpsc::Receiver<TuiCommand>) -> Self {
        self.command_rx = Some(rx);
        self
    }

    /// Non-blocking attempt to receive a pending [`TuiCommand`].
    ///
    /// Returns `None` if no command receiver was attached or the channel is empty.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::{TuiChannel, TuiCommand};
    /// use zeph_tui::event::AgentEvent;
    ///
    /// let (user_tx, user_rx) = mpsc::channel(16);
    /// let (agent_tx, _agent_rx) = mpsc::channel(16);
    /// let (cmd_tx, cmd_rx) = mpsc::channel(8);
    /// cmd_tx.try_send(TuiCommand::SkillList).unwrap();
    /// let mut channel = TuiChannel::new(user_rx, agent_tx).with_command_rx(cmd_rx);
    /// assert_eq!(channel.try_recv_command(), Some(TuiCommand::SkillList));
    /// assert_eq!(channel.try_recv_command(), None);
    /// ```
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
        // Full message is the final rendered response for a turn; losing it leaves the chat
        // panel blank. Use a bounded timeout rather than try_send.
        let event = AgentEvent::FullMessage(text.to_owned());
        match tokio::time::timeout(
            std::time::Duration::from_millis(100),
            self.agent_event_tx.send(event),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(ChannelError::ChannelClosed),
            Err(_elapsed) => {
                tracing::warn!("TuiChannel::send timed out after 100ms, dropping full message");
            }
        }
        Ok(())
    }

    async fn send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        self.accumulated.push_str(chunk);
        // Non-critical: dropping a chunk loses partial streaming output but agent continues.
        let _ = self
            .agent_event_tx
            .try_send(AgentEvent::Chunk(chunk.to_owned()));
        Ok(())
    }

    async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
        // Non-critical: visual signal that streaming ended.
        let _ = self.agent_event_tx.try_send(AgentEvent::Flush);
        Ok(())
    }

    async fn send_typing(&mut self) -> Result<(), ChannelError> {
        // Non-critical: throbber hint only.
        let _ = self.agent_event_tx.try_send(AgentEvent::Typing);
        Ok(())
    }

    async fn send_status(&mut self, text: &str) -> Result<(), ChannelError> {
        // Non-critical: informational status text.
        let _ = self
            .agent_event_tx
            .try_send(AgentEvent::Status(text.to_owned()));
        Ok(())
    }

    async fn send_queue_count(&mut self, count: usize) -> Result<(), ChannelError> {
        // Non-critical: display-only counter.
        let _ = self.agent_event_tx.try_send(AgentEvent::QueueCount(count));
        Ok(())
    }

    async fn send_diff(&mut self, diff: zeph_core::DiffData) -> Result<(), ChannelError> {
        // Substantive user-facing content: bounded send so the diff is displayed unless
        // the TUI is severely stalled (>100ms). On timeout, drop silently; on channel
        // close, propagate the error.
        match tokio::time::timeout(
            std::time::Duration::from_millis(100),
            self.agent_event_tx.send(AgentEvent::DiffReady(diff)),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(ChannelError::ChannelClosed),
            Err(_elapsed) => {
                tracing::warn!("TuiChannel::send_diff timed out after 100ms, dropping diff");
            }
        }
        Ok(())
    }

    async fn send_tool_start(&mut self, event: ToolStartEvent) -> Result<(), ChannelError> {
        let command = event
            .params
            .as_ref()
            .and_then(|p| {
                p.get("command")
                    .or_else(|| p.get("path"))
                    .or_else(|| p.get("url"))
            })
            .and_then(|v| v.as_str())
            .unwrap_or(event.tool_name.as_str())
            .to_owned();
        // Non-critical: visual indicator only.
        let _ = self.agent_event_tx.try_send(AgentEvent::ToolStart {
            tool_name: event.tool_name,
            command,
        });
        Ok(())
    }

    async fn send_tool_output(&mut self, event: ToolOutputEvent) -> Result<(), ChannelError> {
        tracing::debug!(
            tool_name = %event.tool_name.as_str(),
            has_diff = event.diff.is_some(),
            "TuiChannel::send_tool_output called"
        );
        // Substantive user-facing content: bounded send so the output is displayed unless
        // the TUI is severely stalled (>100ms). On timeout, drop silently; on channel
        // close, propagate the error.
        let agent_event = AgentEvent::ToolOutput {
            tool_name: event.tool_name,
            command: event.display.clone(),
            output: event.display.clone(),
            success: !event.is_error,
            diff: event.diff,
            filter_stats: event.filter_stats,
            kept_lines: event.kept_lines,
        };
        match tokio::time::timeout(
            std::time::Duration::from_millis(100),
            self.agent_event_tx.send(agent_event),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(ChannelError::ChannelClosed),
            Err(_elapsed) => {
                tracing::warn!(
                    "TuiChannel::send_tool_output timed out after 100ms, dropping output"
                );
            }
        }
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

    async fn elicit(
        &mut self,
        request: ElicitationRequest,
    ) -> Result<ElicitationResponse, ChannelError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.agent_event_tx
            .send(AgentEvent::ElicitationRequest {
                request,
                response_tx: tx,
            })
            .await
            .map_err(|_| ChannelError::ChannelClosed)?;
        rx.await.map_err(|_| ChannelError::ChannelClosed)
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
    async fn send_tool_start_forwards_event_with_command_from_params() {
        use zeph_core::channel::ToolStartEvent;
        let (mut ch, _user_tx, mut agent_rx) = make_channel();
        ch.send_tool_start(ToolStartEvent {
            tool_name: "bash".into(),
            tool_call_id: "id1".into(),
            params: Some(serde_json::json!({"command": "ls -la"})),
            parent_tool_use_id: None,
            started_at: std::time::Instant::now(),
        })
        .await
        .unwrap();
        let evt = agent_rx.recv().await.unwrap();
        assert!(
            matches!(evt, AgentEvent::ToolStart { ref tool_name, ref command }
                if tool_name == "bash" && command == "ls -la"),
            "expected ToolStart with command from params"
        );
    }

    #[tokio::test]
    async fn send_tool_start_falls_back_to_tool_name() {
        use zeph_core::channel::ToolStartEvent;
        let (mut ch, _user_tx, mut agent_rx) = make_channel();
        ch.send_tool_start(ToolStartEvent {
            tool_name: "memory_search".into(),
            tool_call_id: "id2".into(),
            params: None,
            parent_tool_use_id: None,
            started_at: std::time::Instant::now(),
        })
        .await
        .unwrap();
        let evt = agent_rx.recv().await.unwrap();
        assert!(
            matches!(evt, AgentEvent::ToolStart { ref tool_name, ref command }
                if tool_name == "memory_search" && command == "memory_search"),
            "expected ToolStart with tool_name as fallback command"
        );
    }

    #[tokio::test]
    async fn send_tool_output_bundles_diff_atomically() {
        use zeph_core::channel::ToolOutputEvent;
        let (mut ch, _user_tx, mut agent_rx) = make_channel();
        let diff = zeph_core::DiffData {
            file_path: "src/main.rs".into(),
            old_content: "old".into(),
            new_content: "new".into(),
        };
        ch.send_tool_output(ToolOutputEvent {
            tool_name: "bash".into(),
            display: "[tool output: bash]\n```\nok\n```".into(),
            diff: Some(diff),
            filter_stats: None,
            kept_lines: None,
            locations: None,
            tool_call_id: "".into(),

            terminal_id: None,
            is_error: false,
            parent_tool_use_id: None,
            raw_response: None,
            started_at: None,
        })
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
        use zeph_core::channel::ToolOutputEvent;
        let (mut ch, _user_tx, mut agent_rx) = make_channel();
        ch.send_tool_output(ToolOutputEvent {
            tool_name: "read".into(),
            display: "[tool output: read]\n```\ncontent\n```".into(),
            diff: None,
            filter_stats: None,
            kept_lines: None,
            locations: None,
            tool_call_id: "".into(),

            terminal_id: None,
            is_error: false,
            parent_tool_use_id: None,
            raw_response: None,
            started_at: None,
        })
        .await
        .unwrap();

        let evt = agent_rx.recv().await.unwrap();
        assert!(
            matches!(evt, AgentEvent::ToolOutput { ref tool_name, .. } if tool_name == "read"),
            "expected ToolOutput"
        );
    }

    /// Verify that non-critical send methods return `Ok(())` without blocking
    /// when the channel is full (backpressure test).
    #[tokio::test]
    async fn non_critical_send_returns_ok_when_channel_full() {
        // Capacity 1 — fill it, then try to send non-critical events.
        let (user_tx, user_rx) = mpsc::channel(16);
        let (agent_tx, _agent_rx) = mpsc::channel::<AgentEvent>(1);
        let mut ch = TuiChannel::new(user_rx, agent_tx.clone());
        // Drop user_rx handle we kept only to satisfy make_channel API.
        drop(user_tx);

        // Fill channel to capacity.
        agent_tx
            .try_send(AgentEvent::Typing)
            .expect("channel has capacity 1");

        // All non-critical methods must return Ok(()) immediately even though the channel is full.
        assert!(
            ch.send("hello").await.is_ok(),
            "send should not block or error"
        );
        assert!(
            ch.send_chunk("chunk").await.is_ok(),
            "send_chunk should not block or error"
        );
        assert!(
            ch.flush_chunks().await.is_ok(),
            "flush_chunks should not block or error"
        );
        assert!(
            ch.send_typing().await.is_ok(),
            "send_typing should not block or error"
        );
        assert!(
            ch.send_status("status").await.is_ok(),
            "send_status should not block or error"
        );
        assert!(
            ch.send_queue_count(3).await.is_ok(),
            "send_queue_count should not block or error"
        );
    }
}
