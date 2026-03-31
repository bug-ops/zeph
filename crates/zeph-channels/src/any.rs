// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_core::channel::{
    Channel, ChannelError, ChannelMessage, ElicitationRequest, ElicitationResponse, StopHint,
    ToolOutputEvent, ToolStartEvent,
};

use crate::cli::CliChannel;
#[cfg(feature = "discord")]
use crate::discord::DiscordChannel;
#[cfg(feature = "slack")]
use crate::slack::SlackChannel;
use crate::telegram::TelegramChannel;

/// Enum dispatch for runtime channel selection.
#[derive(Debug)]
pub enum AnyChannel {
    Cli(CliChannel),
    Telegram(TelegramChannel),
    #[cfg(feature = "discord")]
    Discord(DiscordChannel),
    #[cfg(feature = "slack")]
    Slack(SlackChannel),
}

macro_rules! dispatch_channel {
    ($self:expr, $method:ident $(, $arg:expr)*) => {
        match $self {
            AnyChannel::Cli(c) => c.$method($($arg),*).await,
            AnyChannel::Telegram(c) => c.$method($($arg),*).await,
            #[cfg(feature = "discord")]
            AnyChannel::Discord(c) => c.$method($($arg),*).await,
            #[cfg(feature = "slack")]
            AnyChannel::Slack(c) => c.$method($($arg),*).await,
        }
    };
}

impl Channel for AnyChannel {
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        dispatch_channel!(self, recv)
    }

    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        dispatch_channel!(self, send, text)
    }

    async fn send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        dispatch_channel!(self, send_chunk, chunk)
    }

    async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
        dispatch_channel!(self, flush_chunks)
    }

    async fn send_typing(&mut self) -> Result<(), ChannelError> {
        dispatch_channel!(self, send_typing)
    }

    async fn confirm(&mut self, prompt: &str) -> Result<bool, ChannelError> {
        dispatch_channel!(self, confirm, prompt)
    }

    async fn elicit(
        &mut self,
        request: ElicitationRequest,
    ) -> Result<ElicitationResponse, ChannelError> {
        dispatch_channel!(self, elicit, request)
    }

    fn try_recv(&mut self) -> Option<ChannelMessage> {
        match self {
            Self::Cli(c) => c.try_recv(),
            Self::Telegram(c) => c.try_recv(),
            #[cfg(feature = "discord")]
            Self::Discord(c) => c.try_recv(),
            #[cfg(feature = "slack")]
            Self::Slack(c) => c.try_recv(),
        }
    }

    fn supports_exit(&self) -> bool {
        match self {
            Self::Cli(c) => c.supports_exit(),
            Self::Telegram(c) => c.supports_exit(),
            #[cfg(feature = "discord")]
            Self::Discord(c) => c.supports_exit(),
            #[cfg(feature = "slack")]
            Self::Slack(c) => c.supports_exit(),
        }
    }

    async fn send_status(&mut self, text: &str) -> Result<(), ChannelError> {
        dispatch_channel!(self, send_status, text)
    }

    async fn send_queue_count(&mut self, count: usize) -> Result<(), ChannelError> {
        dispatch_channel!(self, send_queue_count, count)
    }

    async fn send_diff(&mut self, diff: zeph_core::DiffData) -> Result<(), ChannelError> {
        dispatch_channel!(self, send_diff, diff)
    }

    async fn send_tool_output(&mut self, event: ToolOutputEvent<'_>) -> Result<(), ChannelError> {
        dispatch_channel!(self, send_tool_output, event)
    }

    async fn send_thinking_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        dispatch_channel!(self, send_thinking_chunk, chunk)
    }

    async fn send_stop_hint(&mut self, hint: StopHint) -> Result<(), ChannelError> {
        dispatch_channel!(self, send_stop_hint, hint)
    }

    async fn send_usage(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        context_window: u64,
    ) -> Result<(), ChannelError> {
        dispatch_channel!(
            self,
            send_usage,
            input_tokens,
            output_tokens,
            context_window
        )
    }

    async fn send_tool_start(&mut self, event: ToolStartEvent<'_>) -> Result<(), ChannelError> {
        dispatch_channel!(self, send_tool_start, event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::CliChannel;
    use zeph_core::channel::Channel;

    #[tokio::test]
    async fn any_channel_cli_send_returns_ok() {
        let mut ch = AnyChannel::Cli(CliChannel::new());
        assert!(ch.send("hello").await.is_ok());
    }

    #[tokio::test]
    async fn any_channel_cli_send_chunk_returns_ok() {
        let mut ch = AnyChannel::Cli(CliChannel::new());
        assert!(ch.send_chunk("chunk").await.is_ok());
    }

    #[tokio::test]
    async fn any_channel_cli_flush_chunks_returns_ok() {
        let mut ch = AnyChannel::Cli(CliChannel::new());
        ch.send_chunk("data").await.unwrap();
        assert!(ch.flush_chunks().await.is_ok());
    }

    #[tokio::test]
    async fn any_channel_cli_send_typing_returns_ok() {
        let mut ch = AnyChannel::Cli(CliChannel::new());
        assert!(ch.send_typing().await.is_ok());
    }

    #[tokio::test]
    async fn any_channel_cli_send_status_returns_ok() {
        let mut ch = AnyChannel::Cli(CliChannel::new());
        assert!(ch.send_status("thinking...").await.is_ok());
    }

    // crossterm on Windows uses ReadConsoleInputW which blocks indefinitely
    // without a real console handle (headless CI), while Unix poll() gets EOF
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn any_channel_cli_confirm_returns_bool() {
        let mut ch = AnyChannel::Cli(CliChannel::new());
        let _ = ch.confirm("confirm?").await;
    }

    #[test]
    fn any_channel_cli_try_recv_returns_none() {
        let mut ch = AnyChannel::Cli(CliChannel::new());
        assert!(ch.try_recv().is_none());
    }

    #[test]
    fn any_channel_debug() {
        let ch = AnyChannel::Cli(CliChannel::new());
        let debug = format!("{ch:?}");
        assert!(debug.contains("Cli"));
    }

    #[tokio::test]
    async fn any_channel_sends_thinking_chunk() {
        let mut ch = AnyChannel::Cli(CliChannel::new());
        assert!(ch.send_thinking_chunk("thinking...").await.is_ok());
    }

    #[tokio::test]
    async fn any_channel_sends_stop_hint() {
        use zeph_core::channel::StopHint;
        let mut ch = AnyChannel::Cli(CliChannel::new());
        assert!(ch.send_stop_hint(StopHint::MaxTokens).await.is_ok());
    }

    #[tokio::test]
    async fn any_channel_sends_usage() {
        let mut ch = AnyChannel::Cli(CliChannel::new());
        assert!(ch.send_usage(100, 50, 200_000).await.is_ok());
    }

    #[tokio::test]
    async fn any_channel_sends_tool_start() {
        use zeph_core::channel::ToolStartEvent;
        let mut ch = AnyChannel::Cli(CliChannel::new());
        assert!(
            ch.send_tool_start(ToolStartEvent {
                tool_name: "shell",
                tool_call_id: "tc-001",
                params: None,
                parent_tool_use_id: None,
            })
            .await
            .is_ok()
        );
    }

    /// Exhaustive `Channel` method coverage for `AnyChannel`.
    ///
    /// When a new method is added to the Channel trait, it must be called here.
    /// If a forwarding is missing in `AnyChannel`, this test serves as a manual checklist
    /// to catch the gap during review.
    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn any_channel_forwards_all_channel_methods() {
        use zeph_core::channel::{StopHint, ToolOutputEvent, ToolStartEvent};

        let mut ch = AnyChannel::Cli(CliChannel::new());
        // 1. recv — skipped (blocks on stdin)
        // 2. try_recv
        let _ = ch.try_recv();
        // 3. supports_exit
        let _ = ch.supports_exit();
        // 4. send
        ch.send("test").await.unwrap();
        // 5. send_chunk
        ch.send_chunk("chunk").await.unwrap();
        // 6. flush_chunks
        ch.flush_chunks().await.unwrap();
        // 7. send_typing
        ch.send_typing().await.unwrap();
        // 8. send_status
        ch.send_status("working").await.unwrap();
        // 9. send_thinking_chunk
        ch.send_thinking_chunk("...").await.unwrap();
        // 10. send_queue_count
        ch.send_queue_count(3).await.unwrap();
        // 11. send_usage
        ch.send_usage(10, 5, 8192).await.unwrap();
        // 12. send_diff
        ch.send_diff(zeph_core::DiffData {
            file_path: String::new(),
            old_content: String::new(),
            new_content: String::new(),
        })
        .await
        .unwrap();
        // 13. send_tool_start
        ch.send_tool_start(ToolStartEvent {
            tool_name: "bash",
            tool_call_id: "x",
            params: None,
            parent_tool_use_id: None,
        })
        .await
        .unwrap();
        // 14. send_tool_output
        ch.send_tool_output(ToolOutputEvent {
            tool_name: "bash",
            body: "ok",
            diff: None,
            filter_stats: None,
            kept_lines: None,
            locations: None,
            tool_call_id: "x",
            is_error: false,
            parent_tool_use_id: None,
            raw_response: None,
            started_at: None,
        })
        .await
        .unwrap();
        // 15. send_stop_hint
        ch.send_stop_hint(StopHint::MaxTurnRequests).await.unwrap();
        // 16. confirm — skipped (reads from stdin; covered by separate test)
    }
}
