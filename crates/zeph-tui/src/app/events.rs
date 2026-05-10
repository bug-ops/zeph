// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use tokio::sync::mpsc;

use crate::event::{AgentEvent, AppEvent};

use super::{App, ChatMessage, ConfirmState, ElicitationState, MessageRole, debug};

impl App {
    /// Dispatch a top-level [`AppEvent`] to the appropriate handler.
    ///
    /// Called once per event in the main [`crate::run_tui`] loop.
    pub fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Key(key) => self.handle_key(key),
            AppEvent::Tick => {
                self.throbber_state.calc_next();
            }
            AppEvent::Resize(_, _) => {
                self.sessions.current_mut().render_cache.clear();
            }
            AppEvent::Agent(agent_event) => self.handle_agent_event(agent_event),
            AppEvent::Paste(text) => self.handle_paste(&text),
        }
    }

    /// Await the next [`AgentEvent`] from the agent channel.
    ///
    /// Returns `None` when all senders have been dropped (agent exited).
    /// Called from the `select!` block in [`crate::run_tui`].
    pub fn poll_agent_event(&mut self) -> impl Future<Output = Option<AgentEvent>> + use<'_> {
        self.agent_event_rx.recv()
    }

    /// Non-blocking poll for a pending [`AgentEvent`].
    ///
    /// Used to drain the channel after a first event has been received,
    /// coalescing multiple events into a single render frame.
    ///
    /// # Errors
    ///
    /// Returns `TryRecvError::Empty` if no events are pending, or
    /// `TryRecvError::Disconnected` if the sender has been dropped.
    pub fn try_recv_agent_event(&mut self) -> Result<AgentEvent, mpsc::error::TryRecvError> {
        self.agent_event_rx.try_recv()
    }

    /// Handle an [`AgentEvent`] and update widget state accordingly.
    ///
    /// This is the main state-transition function for agent-driven updates:
    /// appending streaming chunks, recording tool events, displaying confirm
    /// dialogs, and wiring late-bound channels (cancel signal, metrics).
    #[allow(clippy::too_many_lines)] // large match over all agent event variants
    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Chunk(text) => {
                self.sessions.current_mut().status_label = None;
                if let Some(last) = self.sessions.current_mut().messages.last_mut()
                    && last.role == MessageRole::Assistant
                    && last.streaming
                {
                    last.content.push_str(&text);
                } else {
                    self.sessions
                        .current_mut()
                        .messages
                        .push(ChatMessage::new(MessageRole::Assistant, text).streaming());
                    self.trim_messages();
                }
                // No explicit cache invalidation needed: the cache key includes
                // content_hash, so new chunk content causes a natural cache miss.
                self.auto_scroll();
            }
            AgentEvent::FullMessage(text) => {
                self.sessions.current_mut().status_label = None;
                if !text.starts_with("[tool output") {
                    self.sessions
                        .current_mut()
                        .messages
                        .push(ChatMessage::new(MessageRole::Assistant, text));
                    self.trim_messages();
                }
                self.auto_scroll();
            }
            AgentEvent::Flush => {
                if let Some(last) = self.sessions.current_mut().messages.last_mut()
                    && last.streaming
                {
                    last.streaming = false;
                    let last_idx = self.sessions.current().messages.len().saturating_sub(1);
                    self.sessions
                        .current_mut()
                        .render_cache
                        .invalidate(last_idx);
                }
            }
            AgentEvent::Typing => {
                self.pending_count = self.pending_count.saturating_sub(1);
                self.sessions.current_mut().status_label = Some("thinking...".to_owned());
            }
            AgentEvent::Status(text) => {
                self.sessions.current_mut().status_label =
                    if text.is_empty() { None } else { Some(text) };
                self.auto_scroll();
            }
            AgentEvent::ToolStart {
                tool_name,
                command,
                tool_call_id,
            } => {
                self.sessions.current_mut().status_label = None;
                self.sessions.current_mut().messages.push(
                    ChatMessage::new(MessageRole::Tool, format!("$ {command}\n"))
                        .streaming()
                        .with_tool(tool_name)
                        .with_tool_call_id(tool_call_id),
                );
                self.trim_messages();
                self.auto_scroll();
            }
            AgentEvent::ToolOutputChunk {
                chunk,
                tool_call_id,
                ..
            } => {
                let pos = if tool_call_id.is_empty() {
                    // Shell tool chunks arrive without a tool_call_id; fall back to the last
                    // streaming Tool message (there is at most one active at a time).
                    self.sessions
                        .current()
                        .messages
                        .iter()
                        .rposition(|m| m.role == MessageRole::Tool && m.streaming)
                } else {
                    let found =
                        self.sessions.current().messages.iter().rposition(|m| {
                            m.tool_call_id.as_deref() == Some(tool_call_id.as_str())
                        });
                    if found.is_none() {
                        tracing::warn!(
                            %tool_call_id,
                            "ToolOutputChunk: no message with matching tool_call_id — dropping chunk"
                        );
                    }
                    found
                };
                if let Some(pos) = pos {
                    self.sessions.current_mut().messages[pos]
                        .content
                        .push_str(&chunk);
                    self.sessions.current_mut().render_cache.invalidate(pos);
                }
                self.auto_scroll();
            }
            AgentEvent::ToolOutput {
                tool_name,
                output,
                diff,
                filter_stats,
                kept_lines,
                success,
                tool_call_id,
                ..
            } => {
                self.handle_tool_output_event(
                    tool_name,
                    output,
                    diff,
                    filter_stats,
                    kept_lines,
                    success,
                    tool_call_id,
                );
            }
            AgentEvent::ConfirmRequest {
                prompt,
                response_tx,
            } => {
                self.confirm_state = Some(ConfirmState {
                    prompt,
                    response_tx: Some(response_tx),
                });
            }
            AgentEvent::ElicitationRequest {
                request,
                response_tx,
            } => {
                let dialog = crate::widgets::elicitation::ElicitationDialogState::new(request);
                self.elicitation_state = Some(ElicitationState {
                    dialog,
                    response_tx: Some(response_tx),
                });
            }
            AgentEvent::QueueCount(count) => {
                self.queued_count = count;
                self.pending_count = count;
            }
            AgentEvent::DiffReady { diff, tool_call_id } => {
                self.handle_diff_ready(diff, &tool_call_id);
            }
            AgentEvent::CommandResult { output, .. } => {
                self.command_palette = None;
                self.sessions
                    .current_mut()
                    .messages
                    .push(ChatMessage::new(MessageRole::System, output));
                self.trim_messages();
                self.auto_scroll();
            }
            AgentEvent::SetCancelSignal(signal) => {
                self.set_cancel_signal(signal);
            }
            AgentEvent::SetMetricsRx(rx) => {
                self.set_metrics_rx(rx);
            }
        }
    }

    fn handle_diff_ready(&mut self, diff: zeph_core::DiffData, tool_call_id: &str) {
        if let Some(msg) = self
            .sessions
            .current_mut()
            .messages
            .iter_mut()
            .rev()
            .find(|m| {
                m.role == MessageRole::Tool && m.tool_call_id.as_deref() == Some(tool_call_id)
            })
        {
            msg.diff_data = Some(diff);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_tool_output_event(
        &mut self,
        tool_name: zeph_common::ToolName,
        output: String,
        diff: Option<zeph_core::DiffData>,
        filter_stats: Option<String>,
        kept_lines: Option<Vec<usize>>,
        success: bool,
        tool_call_id: String,
    ) {
        debug!(
            %tool_name,
            has_diff = diff.is_some(),
            has_filter_stats = filter_stats.is_some(),
            output_len = output.len(),
            "TUI ToolOutput event received"
        );
        // Try id-based lookup first; fall back to streaming-flag lookup for
        // cases where ToolStart was not emitted (legacy path, empty tool_call_id).
        let pos = if tool_call_id.is_empty() {
            self.sessions
                .current()
                .messages
                .iter()
                .rposition(|m| m.role == MessageRole::Tool && m.streaming)
        } else {
            let found = self
                .sessions
                .current()
                .messages
                .iter()
                .rposition(|m| {
                    m.role == MessageRole::Tool
                        && m.streaming
                        && m.tool_call_id.as_deref() == Some(tool_call_id.as_str())
                })
                .or_else(|| {
                    self.sessions
                        .current()
                        .messages
                        .iter()
                        .rposition(|m| m.role == MessageRole::Tool && m.streaming)
                });
            if found.is_none() {
                tracing::warn!(
                    tool_call_id = %tool_call_id,
                    "ToolOutput: no streaming Tool message found — skipping finalization"
                );
            }
            found
        };

        if let Some(pos) = pos {
            // Finalize existing streaming tool message (shell or native path with ToolStart).
            // Replace content after the header line ("$ cmd\n") with the canonical body_display
            // from ToolOutputEvent. Streaming chunks (Path B) may already occupy that space;
            // appending would duplicate the output. Truncating to the header and re-writing
            // body_display produces exactly one copy regardless of whether chunks arrived.
            debug!("finalizing existing streaming Tool message");
            let header_end = self.sessions.current_mut().messages[pos]
                .content
                .find('\n')
                .map_or(0, |i| i + 1);
            self.sessions.current_mut().messages[pos]
                .content
                .truncate(header_end);
            self.sessions.current_mut().messages[pos]
                .content
                .push_str(&output);
            self.sessions.current_mut().messages[pos].streaming = false;
            self.sessions.current_mut().messages[pos].diff_data = diff;
            self.sessions.current_mut().messages[pos].filter_stats = filter_stats;
            self.sessions.current_mut().messages[pos].kept_lines = kept_lines;
            self.sessions.current_mut().messages[pos].success = Some(success);
            self.sessions.current_mut().render_cache.invalidate(pos);
        } else if diff.is_some() || filter_stats.is_some() || kept_lines.is_some() {
            // No prior ToolStart: create the message now (legacy fallback).
            debug!("creating new Tool message with diff (no prior ToolStart)");
            let mut msg = ChatMessage::new(MessageRole::Tool, output)
                .with_tool(tool_name)
                .with_tool_call_id(tool_call_id);
            msg.diff_data = diff;
            msg.filter_stats = filter_stats;
            msg.kept_lines = kept_lines;
            msg.success = Some(success);
            self.sessions.current_mut().messages.push(msg);
            self.trim_messages();
        } else if let Some(msg) = self
            .sessions
            .current_mut()
            .messages
            .iter_mut()
            .rev()
            .find(|m| m.role == MessageRole::Tool)
        {
            msg.filter_stats = filter_stats;
        }
        self.auto_scroll();
    }

    #[must_use]
    pub fn confirm_state(&self) -> Option<&ConfirmState> {
        self.confirm_state.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use crate::app::App;
    use crate::event::AgentEvent;
    use crate::types::{ChatMessage, MessageRole};
    use zeph_core::DiffData;

    fn make_app() -> App {
        let (user_tx, agent_rx) = {
            let (utx, _urx) = mpsc::channel(8);
            let (_atx, arx) = mpsc::channel(8);
            (utx, arx)
        };
        let mut app = App::new(user_tx, agent_rx);
        app.sessions.current_mut().messages.clear();
        app
    }

    /// Push a streaming Tool message with a specific `tool_call_id` directly onto the session.
    fn push_tool_msg(app: &mut App, id: &str) {
        let msg = ChatMessage::new(MessageRole::Tool, format!("$ cmd_{id}\n"))
            .streaming()
            .with_tool_call_id(id.to_owned());
        app.sessions.current_mut().messages.push(msg);
    }

    fn tool_msg(id: &str) -> ChatMessage {
        ChatMessage::new(MessageRole::Tool, "$ cmd\n".to_owned())
            .with_tool("bash".into())
            .with_tool_call_id(id.to_owned())
    }

    fn diff() -> DiffData {
        DiffData {
            file_path: "a.rs".into(),
            old_content: "old".into(),
            new_content: "new".into(),
        }
    }

    #[test]
    fn tool_output_chunk_routes_by_id_out_of_order() {
        let mut app = make_app();
        push_tool_msg(&mut app, "a");
        push_tool_msg(&mut app, "b");
        push_tool_msg(&mut app, "c");

        // Deliver chunks out of order: c, a, b, a, c
        for (id, chunk) in [
            ("c", "c1"),
            ("a", "a1"),
            ("b", "b1"),
            ("a", "a2"),
            ("c", "c2"),
        ] {
            app.handle_agent_event(AgentEvent::ToolOutputChunk {
                tool_name: "bash".into(),
                command: String::new(),
                chunk: chunk.to_owned(),
                tool_call_id: id.to_owned(),
            });
        }

        let msgs = app.messages();
        assert_eq!(msgs.len(), 3);
        // Message order: a=0, b=1, c=2
        assert_eq!(msgs[0].content, "$ cmd_a\na1a2");
        assert_eq!(msgs[1].content, "$ cmd_b\nb1");
        assert_eq!(msgs[2].content, "$ cmd_c\nc1c2");
    }

    #[test]
    fn tool_output_chunk_with_unknown_id_is_dropped() {
        let mut app = make_app();
        push_tool_msg(&mut app, "known");

        // Chunk for an id that has no matching message — must be silently dropped.
        app.handle_agent_event(AgentEvent::ToolOutputChunk {
            tool_name: "bash".into(),
            command: String::new(),
            chunk: "should-not-appear".to_owned(),
            tool_call_id: "unknown-xyz".to_owned(),
        });

        // The known message must be unchanged.
        assert_eq!(app.messages().len(), 1);
        assert_eq!(app.messages()[0].content, "$ cmd_known\n");
    }

    #[test]
    fn tool_output_finalizes_correct_message_by_id() {
        let mut app = make_app();
        push_tool_msg(&mut app, "t1");
        push_tool_msg(&mut app, "t2");

        // Finalize t1 with ToolOutput.
        app.handle_agent_event(AgentEvent::ToolOutput {
            tool_name: "bash".into(),
            command: "$ cmd_t1\n".into(),
            output: "final-output-t1".to_owned(),
            success: true,
            diff: None,
            filter_stats: None,
            kept_lines: None,
            tool_call_id: "t1".to_owned(),
        });

        let msgs = app.messages();
        assert_eq!(msgs.len(), 2);
        // t1 must be finalized (not streaming) with the canonical output.
        assert!(!msgs[0].streaming);
        assert!(msgs[0].content.contains("final-output-t1"));
        // t2 must still be streaming and unchanged.
        assert!(msgs[1].streaming);
        assert_eq!(msgs[1].content, "$ cmd_t2\n");
    }

    #[test]
    fn diff_ready_attaches_to_matching_id() {
        let mut app = make_app();
        app.sessions.current_mut().messages.push(tool_msg("call-1"));
        app.sessions.current_mut().messages.push(tool_msg("call-2"));

        app.handle_agent_event(AgentEvent::DiffReady {
            diff: diff(),
            tool_call_id: "call-2".into(),
        });

        assert!(app.sessions.current().messages[0].diff_data.is_none());
        assert!(app.sessions.current().messages[1].diff_data.is_some());
    }

    #[test]
    fn diff_ready_mismatched_id_does_not_attach() {
        let mut app = make_app();
        app.sessions.current_mut().messages.push(tool_msg("call-1"));

        app.handle_agent_event(AgentEvent::DiffReady {
            diff: diff(),
            tool_call_id: "call-99".into(),
        });

        assert!(app.sessions.current().messages[0].diff_data.is_none());
    }

    #[test]
    fn diff_ready_empty_id_does_not_attach() {
        let mut app = make_app();
        app.sessions.current_mut().messages.push(tool_msg("call-1"));

        app.handle_agent_event(AgentEvent::DiffReady {
            diff: diff(),
            tool_call_id: String::new(),
        });

        assert!(app.sessions.current().messages[0].diff_data.is_none());
    }

    #[test]
    fn diff_ready_two_concurrent_attach_to_correct_messages() {
        let mut app = make_app();
        app.sessions.current_mut().messages.push(tool_msg("call-A"));
        app.sessions.current_mut().messages.push(tool_msg("call-B"));
        app.sessions.current_mut().messages.push(tool_msg("call-C"));

        let diff_a = DiffData {
            file_path: "a.rs".into(),
            old_content: "old_a".into(),
            new_content: "new_a".into(),
        };
        let diff_b = DiffData {
            file_path: "b.rs".into(),
            old_content: "old_b".into(),
            new_content: "new_b".into(),
        };

        // Deliver out of order: B first, then A
        app.handle_agent_event(AgentEvent::DiffReady {
            diff: diff_b,
            tool_call_id: "call-B".into(),
        });
        app.handle_agent_event(AgentEvent::DiffReady {
            diff: diff_a,
            tool_call_id: "call-A".into(),
        });

        let msgs = &app.sessions.current().messages;
        assert_eq!(
            msgs[0].diff_data.as_ref().map(|d| d.file_path.as_str()),
            Some("a.rs"),
            "call-A diff must attach to message 0"
        );
        assert_eq!(
            msgs[1].diff_data.as_ref().map(|d| d.file_path.as_str()),
            Some("b.rs"),
            "call-B diff must attach to message 1"
        );
        assert!(
            msgs[2].diff_data.is_none(),
            "call-C must remain without diff"
        );
    }
}
