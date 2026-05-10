// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`AgentChannelView`] — adapter from [`crate::channel::Channel`] to
//! [`zeph_agent_tools::AgentChannel`].
//!
//! This is the single authorized implementation of the sealed `AgentChannel` trait.
//! It wraps `&'a mut C` so dispatcher code in `zeph-agent-tools` can be invoked across
//! the crate boundary without owning the channel or imposing additional `Send`/`Sync`
//! bounds on `Channel` implementations.
//!
//! # Design
//!
//! The adapter is orphan-safe because `AgentChannelView` is a local type in `zeph-core`.
//! Construct it at the dispatch boundary with [`AgentChannelView::new`], invoke dispatcher
//! methods through the `AgentChannel` trait, then drop the view to release the borrow.

use std::time::Instant;

use zeph_agent_tools::channel::{AgentChannel, ChannelSinkError, ToolEventOutput, ToolEventStart};
use zeph_agent_tools::sealed::Sealed;

use crate::channel::{Channel, StopHint, ToolOutputEvent, ToolStartEvent};

/// Borrowed adapter that wraps `&mut C` so any [`Channel`] implementation can be used
/// where [`zeph_agent_tools::AgentChannel`] is required.
///
/// Constructed at the dispatch boundary. Holds no state other than the borrowed channel.
///
// TODO(review): AgentChannelView has no callers yet — the dispatcher extraction that will
// consume it is deferred per zeph-agent-tools/lib.rs:17-20. Remove this allow once #3516
// lands and the dispatcher moves to zeph-agent-tools.
#[allow(dead_code)]
pub(crate) struct AgentChannelView<'a, C: Channel> {
    channel: &'a mut C,
}

impl<'a, C: Channel> AgentChannelView<'a, C> {
    /// Wrap a mutable channel reference as an [`AgentChannelView`].
    #[allow(dead_code)]
    pub(crate) fn new(channel: &'a mut C) -> Self {
        Self { channel }
    }
}

impl<C: Channel> Sealed for AgentChannelView<'_, C> {}

impl<C: Channel + Send> AgentChannel for AgentChannelView<'_, C> {
    async fn send(&mut self, text: &str) -> Result<(), ChannelSinkError> {
        self.channel
            .send(text)
            .await
            .map_err(|e| ChannelSinkError::new(e.to_string()))
    }

    async fn send_status(&mut self, text: &str) -> Result<(), ChannelSinkError> {
        self.channel
            .send_status(text)
            .await
            .map_err(|e| ChannelSinkError::new(e.to_string()))
    }

    async fn send_typing(&mut self) -> Result<(), ChannelSinkError> {
        self.channel
            .send_typing()
            .await
            .map_err(|e| ChannelSinkError::new(e.to_string()))
    }

    async fn flush_chunks(&mut self) -> Result<(), ChannelSinkError> {
        self.channel
            .flush_chunks()
            .await
            .map_err(|e| ChannelSinkError::new(e.to_string()))
    }

    async fn confirm(&mut self, prompt: &str) -> Result<bool, ChannelSinkError> {
        self.channel
            .confirm(prompt)
            .await
            .map_err(|e| ChannelSinkError::new(e.to_string()))
    }

    async fn send_stop_hint(&mut self, reason: &str) -> Result<(), ChannelSinkError> {
        // TODO(review): if new StopHint variants are added to zeph-core::channel::StopHint,
        // mirror them here AND at any dispatcher emit-sites in zeph-agent-tools.
        let hint = match reason {
            "max_tokens" => StopHint::MaxTokens,
            "max_turn_requests" => StopHint::MaxTurnRequests,
            other => {
                tracing::warn!(
                    reason = other,
                    "AgentChannelView: unknown stop reason, ignoring"
                );
                return Ok(());
            }
        };
        self.channel
            .send_stop_hint(hint)
            .await
            .map_err(|e| ChannelSinkError::new(e.to_string()))
    }

    async fn send_tool_start(&mut self, event: ToolEventStart<'_>) -> Result<(), ChannelSinkError> {
        let canonical = ToolStartEvent {
            tool_name: zeph_common::ToolName::new(event.tool_name),
            tool_call_id: event.tool_use_id.to_owned(),
            params: event
                .args_summary
                .map(|s| serde_json::Value::String(s.to_owned())),
            parent_tool_use_id: event.parent_id.map(str::to_owned),
            started_at: Instant::now(),
            speculative: false,
            sandbox_profile: None,
        };
        self.channel
            .send_tool_start(canonical)
            .await
            .map_err(|e| ChannelSinkError::new(e.to_string()))
    }

    async fn send_tool_output(
        &mut self,
        event: ToolEventOutput<'_>,
    ) -> Result<(), ChannelSinkError> {
        let canonical = ToolOutputEvent {
            tool_name: zeph_common::ToolName::new(event.tool_name),
            display: event.body.to_owned(),
            diff: None,
            filter_stats: None,
            kept_lines: None,
            locations: None,
            tool_call_id: event.tool_use_id.to_owned(),
            terminal_id: None,
            is_error: event.is_error,
            parent_tool_use_id: None,
            raw_response: None,
            started_at: None,
        };
        self.channel
            .send_tool_output(canonical)
            .await
            .map_err(|e| ChannelSinkError::new(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{ChannelMessage, LoopbackChannel, LoopbackEvent};

    #[tokio::test]
    async fn agent_channel_view_forwards_send() {
        let (mut ch, mut handle) = LoopbackChannel::pair(8);
        let mut view = AgentChannelView::new(&mut ch);
        view.send("hi").await.unwrap();
        let event = handle.output_rx.recv().await.unwrap();
        assert!(matches!(event, LoopbackEvent::FullMessage(m) if m == "hi"));
    }

    #[tokio::test]
    async fn agent_channel_view_forwards_send_status() {
        let (mut ch, mut handle) = LoopbackChannel::pair(8);
        let mut view = AgentChannelView::new(&mut ch);
        view.send_status("working...").await.unwrap();
        let event = handle.output_rx.recv().await.unwrap();
        assert!(matches!(event, LoopbackEvent::Status(s) if s == "working..."));
    }

    #[tokio::test]
    async fn agent_channel_view_forwards_flush_chunks() {
        let (mut ch, mut handle) = LoopbackChannel::pair(8);
        let mut view = AgentChannelView::new(&mut ch);
        view.flush_chunks().await.unwrap();
        let event = handle.output_rx.recv().await.unwrap();
        assert!(matches!(event, LoopbackEvent::Flush));
    }

    #[tokio::test]
    async fn agent_channel_view_confirm_auto_approves() {
        let (mut ch, _handle) = LoopbackChannel::pair(8);
        let mut view = AgentChannelView::new(&mut ch);
        let result = view.confirm("proceed?").await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn agent_channel_view_send_stop_hint_max_tokens() {
        let (mut ch, mut handle) = LoopbackChannel::pair(8);
        let mut view = AgentChannelView::new(&mut ch);
        view.send_stop_hint("max_tokens").await.unwrap();
        let event = handle.output_rx.recv().await.unwrap();
        assert!(matches!(event, LoopbackEvent::Stop(StopHint::MaxTokens)));
    }

    #[tokio::test]
    async fn agent_channel_view_send_stop_hint_unknown_is_noop() {
        let (mut ch, _handle) = LoopbackChannel::pair(8);
        let mut view = AgentChannelView::new(&mut ch);
        // Unknown reason should not send any event
        view.send_stop_hint("unknown_reason").await.unwrap();
    }

    #[tokio::test]
    async fn agent_channel_view_forwards_tool_output() {
        use zeph_agent_tools::channel::ToolEventOutput;

        let (mut ch, mut handle) = LoopbackChannel::pair(8);
        let mut view = AgentChannelView::new(&mut ch);
        let event = ToolEventOutput {
            tool_name: "bash",
            tool_use_id: "tc-001",
            body: "exit 0",
            is_error: false,
            streamed: false,
        };
        view.send_tool_output(event).await.unwrap();
        let ev = handle.output_rx.recv().await.unwrap();
        match ev {
            LoopbackEvent::ToolOutput(data) => {
                assert_eq!(data.tool_name.as_str(), "bash");
                assert_eq!(data.display, "exit 0");
                assert!(!data.is_error);
            }
            _ => panic!("expected ToolOutput event"),
        }
    }

    // Verify construction doesn't require the channel to be consumed.
    #[test]
    fn agent_channel_view_does_not_move_channel() {
        let (mut ch, _handle) = LoopbackChannel::pair(8);
        {
            let _view = AgentChannelView::new(&mut ch);
        }
        // ch still accessible after view is dropped
        drop(ch);
    }

    // Suppress unused field warning for completeness of the struct check.
    #[test]
    fn channel_message_roundtrip() {
        let msg = ChannelMessage {
            text: "test".to_owned(),
            attachments: vec![],
            is_guest_context: false,
            is_from_bot: false,
        };
        assert_eq!(msg.text, "test");
    }
}
