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
            AppEvent::MouseScroll(delta) => {
                if self.confirm_state.is_none() {
                    if delta > 0 {
                        self.sessions.current_mut().scroll_offset =
                            self.sessions.current().scroll_offset.saturating_add(1);
                    } else {
                        self.sessions.current_mut().scroll_offset =
                            self.sessions.current().scroll_offset.saturating_sub(1);
                    }
                }
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
            AgentEvent::ToolStart { tool_name, command } => {
                self.sessions.current_mut().status_label = None;
                self.sessions.current_mut().messages.push(
                    ChatMessage::new(MessageRole::Tool, format!("$ {command}\n"))
                        .streaming()
                        .with_tool(tool_name),
                );
                self.trim_messages();
                self.auto_scroll();
            }
            AgentEvent::ToolOutputChunk { chunk, .. } => {
                if let Some(pos) = self
                    .sessions
                    .current_mut()
                    .messages
                    .iter()
                    .rposition(|m| m.role == MessageRole::Tool && m.streaming)
                {
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
                ..
            } => {
                self.handle_tool_output_event(tool_name, output, diff, filter_stats, kept_lines);
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
            AgentEvent::DiffReady(diff) => self.handle_diff_ready(diff),
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

    fn handle_diff_ready(&mut self, diff: zeph_core::DiffData) {
        if let Some(msg) = self
            .sessions
            .current_mut()
            .messages
            .iter_mut()
            .rev()
            .find(|m| m.role == MessageRole::Tool)
        {
            msg.diff_data = Some(diff);
        }
    }

    fn handle_tool_output_event(
        &mut self,
        tool_name: zeph_common::ToolName,
        output: String,
        diff: Option<zeph_core::DiffData>,
        filter_stats: Option<String>,
        kept_lines: Option<Vec<usize>>,
    ) {
        debug!(
            %tool_name,
            has_diff = diff.is_some(),
            has_filter_stats = filter_stats.is_some(),
            output_len = output.len(),
            "TUI ToolOutput event received"
        );
        if let Some(pos) = self
            .sessions
            .current_mut()
            .messages
            .iter()
            .rposition(|m| m.role == MessageRole::Tool && m.streaming)
        {
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
            self.sessions.current_mut().render_cache.invalidate(pos);
        } else if diff.is_some() || filter_stats.is_some() || kept_lines.is_some() {
            // No prior ToolStart: create the message now (legacy fallback).
            debug!("creating new Tool message with diff (no prior ToolStart)");
            let mut msg = ChatMessage::new(MessageRole::Tool, output).with_tool(tool_name);
            msg.diff_data = diff;
            msg.filter_stats = filter_stats;
            msg.kept_lines = kept_lines;
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
