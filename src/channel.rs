// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use zeph_channels::AnyChannel;
use zeph_channels::CliChannel;
use zeph_channels::JsonCliChannel;
#[cfg(feature = "discord")]
use zeph_channels::discord::DiscordChannel;
#[cfg(feature = "slack")]
use zeph_channels::slack::SlackChannel;
use zeph_channels::telegram::TelegramChannel;
use zeph_common::TaskSupervisor;

use crate::execution_mode::ExecutionMode;
#[cfg(feature = "tui")]
use zeph_core::channel::{
    Channel, ChannelError, ChannelMessage, ElicitationRequest, ElicitationResponse,
    SkillCatalogItem, StopHint, ToolOutputEvent, ToolStartEvent,
};
use zeph_core::config::Config;
use zeph_core::json_event_sink::JsonEventSink;
#[cfg(feature = "tui")]
use zeph_tui::TuiChannel;

pub(crate) type CliHistory = (Vec<String>, Box<dyn Fn(&str) + Send>);

#[cfg(feature = "tui")]
#[derive(Debug)]
pub(crate) enum AppChannel {
    Standard(Box<AnyChannel>),
    Tui(TuiChannel),
}

#[cfg(feature = "tui")]
macro_rules! dispatch_app_channel {
    ($self:expr, $method:ident $(, $arg:expr)*) => {
        match $self {
            AppChannel::Standard(c) => c.$method($($arg),*).await,
            AppChannel::Tui(c) => c.$method($($arg),*).await,
        }
    };
}

#[cfg(feature = "tui")]
impl Channel for AppChannel {
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        dispatch_app_channel!(self, recv)
    }
    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send, text)
    }
    async fn send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_chunk, chunk)
    }
    async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, flush_chunks)
    }
    async fn send_typing(&mut self) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_typing)
    }
    async fn confirm(&mut self, prompt: &str) -> Result<bool, ChannelError> {
        dispatch_app_channel!(self, confirm, prompt)
    }
    async fn elicit(
        &mut self,
        request: ElicitationRequest,
    ) -> Result<ElicitationResponse, ChannelError> {
        dispatch_app_channel!(self, elicit, request)
    }
    fn try_recv(&mut self) -> Option<ChannelMessage> {
        match self {
            Self::Standard(c) => c.try_recv(),
            Self::Tui(c) => c.try_recv(),
        }
    }

    fn supports_exit(&self) -> bool {
        match self {
            Self::Standard(c) => c.supports_exit(),
            Self::Tui(c) => c.supports_exit(),
        }
    }

    fn requires_input_sanitization(&self) -> bool {
        match self {
            Self::Standard(c) => c.requires_input_sanitization(),
            Self::Tui(c) => c.requires_input_sanitization(),
        }
    }
    async fn send_status(&mut self, text: &str) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_status, text)
    }
    async fn send_skill_catalog(&mut self, items: &[SkillCatalogItem]) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_skill_catalog, items)
    }
    async fn send_queue_count(&mut self, count: usize) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_queue_count, count)
    }
    async fn send_context_estimate(&mut self, tokens: usize) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_context_estimate, tokens)
    }
    async fn send_diff(
        &mut self,
        diff: zeph_core::DiffData,
        tool_call_id: &str,
    ) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_diff, diff, tool_call_id)
    }
    async fn send_tool_output(&mut self, event: ToolOutputEvent) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_tool_output, event)
    }

    async fn send_thinking_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_thinking_chunk, chunk)
    }

    async fn send_stop_hint(&mut self, hint: StopHint) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_stop_hint, hint)
    }

    async fn send_usage(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        context_window: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        cost_cents: f64,
    ) -> Result<(), ChannelError> {
        dispatch_app_channel!(
            self,
            send_usage,
            input_tokens,
            output_tokens,
            context_window,
            cache_read_tokens,
            cache_write_tokens,
            cost_cents
        )
    }

    async fn send_tool_start(&mut self, event: ToolStartEvent) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_tool_start, event)
    }

    async fn notify_foreground_subagent_started(
        &mut self,
        id: &str,
        name: &str,
    ) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, notify_foreground_subagent_started, id, name)
    }

    async fn notify_foreground_subagent_completed(
        &mut self,
        id: &str,
        name: &str,
        success: bool,
    ) -> Result<(), ChannelError> {
        dispatch_app_channel!(
            self,
            notify_foreground_subagent_completed,
            id,
            name,
            success
        )
    }
}

#[cfg(feature = "tui")]
pub(crate) struct TuiHandle {
    pub(crate) user_tx: tokio::sync::mpsc::Sender<String>,
    pub(crate) agent_tx: tokio::sync::mpsc::Sender<zeph_tui::AgentEvent>,
    /// Wrapped in `Option` so it can be taken by `start_tui_early` for early TUI rendering.
    pub(crate) agent_rx: Option<tokio::sync::mpsc::Receiver<zeph_tui::AgentEvent>>,
    pub(crate) command_tx: tokio::sync::mpsc::Sender<zeph_tui::TuiCommand>,
    pub(crate) command_rx: tokio::sync::mpsc::Receiver<zeph_tui::TuiCommand>,
}

/// Create a channel and, in JSON mode, return the shared sink so callers can
/// also install a [`zeph_core::json_event_layer::JsonEventLayer`] on the agent.
/// When `supervisor` is `Some`, it is forwarded to channel adapters that support
/// supervised task registration (Telegram, Discord, Slack).
#[allow(clippy::unused_async)]
pub(crate) async fn create_channel_inner(
    config: &Config,
    history: Option<CliHistory>,
    exec_mode: ExecutionMode,
    supervisor: Option<TaskSupervisor>,
) -> anyhow::Result<(AnyChannel, Option<Arc<JsonEventSink>>)> {
    if exec_mode.json {
        let sink = Arc::new(JsonEventSink::new());
        let channel = AnyChannel::JsonCli(JsonCliChannel::new(Arc::clone(&sink), exec_mode.auto));
        return Ok((channel, Some(sink)));
    }
    #[cfg(feature = "discord")]
    if let Some(dc) = &config.discord
        && let Some(token) = &dc.token
    {
        let channel = DiscordChannel::new(
            token.clone(),
            dc.allowed_user_ids.clone(),
            dc.allowed_role_ids.clone(),
            dc.allowed_channel_ids.clone(),
            supervisor.as_ref(),
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        tracing::info!("running in Discord mode");
        return Ok((AnyChannel::Discord(channel), None));
    }

    #[cfg(feature = "slack")]
    if let Some(sl) = &config.slack
        && let Some(bot_token) = &sl.bot_token
    {
        let signing_secret = sl.signing_secret.clone().unwrap_or_default();
        let channel = SlackChannel::new_with_supervisor(
            bot_token.clone(),
            signing_secret,
            sl.webhook_host.clone(),
            sl.port,
            sl.allowed_user_ids.clone(),
            sl.allowed_channel_ids.clone(),
            supervisor.as_ref(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        tracing::info!(
            "running in Slack mode (events on {}:{})",
            sl.webhook_host,
            sl.port
        );
        return Ok((AnyChannel::Slack(channel), None));
    }

    if let Some(token) = config.telegram.as_ref().and_then(|t| t.token.clone()) {
        let tg_cfg = config.telegram.as_ref().unwrap();
        let allowed = tg_cfg.allowed_users.clone();
        let stream_interval = std::time::Duration::from_millis(tg_cfg.stream_interval_ms);
        let mut tg = TelegramChannel::new(token, allowed)
            .with_stream_interval(stream_interval)
            .with_expandable_blockquote_min_lines(tg_cfg.expandable_blockquote_min_lines)
            .with_guest_mode(tg_cfg.guest_mode)
            .with_bot_to_bot(
                tg_cfg.bot_to_bot,
                tg_cfg.allowed_bots.clone(),
                tg_cfg.max_bot_chain_depth,
            );
        if let Some(sup) = supervisor {
            tg = tg.with_supervisor(sup);
        }
        let tg = tg.start()?;
        tracing::info!("running in Telegram mode");
        return Ok((AnyChannel::Telegram(tg), None));
    }

    if let Some((entries, persist_fn)) = history {
        let cli = CliChannel::with_history(entries, persist_fn);
        return Ok((AnyChannel::Cli(cli), None));
    }

    Ok((AnyChannel::Cli(CliChannel::new()), None))
}

#[cfg(feature = "tui")]
pub(crate) async fn create_channel_with_tui(
    config: &Config,
    tui_active: bool,
    history: Option<CliHistory>,
    exec_mode: ExecutionMode,
    supervisor: Option<TaskSupervisor>,
) -> anyhow::Result<(AppChannel, Option<TuiHandle>, Option<Arc<JsonEventSink>>)> {
    if tui_active {
        let (user_tx, user_rx) = tokio::sync::mpsc::channel(32);
        let (agent_tx, agent_rx) = tokio::sync::mpsc::channel(256);
        let agent_tx_clone = agent_tx.clone();
        // command_tx goes to App; command_rx is handled by forward_tui_commands task.
        let (command_tx, command_rx) = tokio::sync::mpsc::channel::<zeph_tui::TuiCommand>(16);
        let channel = TuiChannel::new(user_rx, agent_tx);
        let handle = TuiHandle {
            user_tx,
            agent_tx: agent_tx_clone,
            agent_rx: Some(agent_rx),
            command_tx,
            command_rx,
        };
        return Ok((AppChannel::Tui(channel), Some(handle), None));
    }
    let (channel, sink) = create_channel_inner(config, history, exec_mode, supervisor).await?;
    Ok((AppChannel::Standard(Box::new(channel)), None, sink))
}

#[cfg(test)]
pub(crate) async fn create_channel(config: &Config) -> anyhow::Result<AnyChannel> {
    let (ch, _sink) = create_channel_inner(config, None, ExecutionMode::default(), None).await?;
    Ok(ch)
}

#[cfg(all(test, feature = "tui"))]
mod tests {
    use super::*;
    use zeph_channels::AnyChannel;
    use zeph_channels::CliChannel;
    use zeph_common::ToolName;
    use zeph_core::channel::{Channel, StopHint, ToolStartEvent};

    fn make_app_channel() -> AppChannel {
        AppChannel::Standard(Box::new(AnyChannel::Cli(CliChannel::new())))
    }

    #[tokio::test]
    async fn app_channel_sends_thinking_chunk() {
        let mut ch = make_app_channel();
        assert!(ch.send_thinking_chunk("reasoning...").await.is_ok());
    }

    #[tokio::test]
    async fn app_channel_sends_stop_hint() {
        let mut ch = make_app_channel();
        assert!(ch.send_stop_hint(StopHint::MaxTokens).await.is_ok());
    }

    #[tokio::test]
    async fn app_channel_sends_usage() {
        let mut ch = make_app_channel();
        assert!(ch.send_usage(100, 50, 200_000, 0, 0, 0.0).await.is_ok());
    }

    #[tokio::test]
    async fn app_channel_sends_tool_start() {
        let mut ch = make_app_channel();
        assert!(
            ch.send_tool_start(ToolStartEvent {
                tool_name: ToolName::from("shell"),
                tool_call_id: "tc-001".to_string(),
                params: None,
                parent_tool_use_id: None,
                started_at: std::time::Instant::now(),
                speculative: false,
                sandbox_profile: None,
                is_mcp: false,
            })
            .await
            .is_ok()
        );
    }

    /// Exhaustive `Channel` method coverage for `AppChannel`.
    ///
    /// When a new method is added to the `Channel` trait, it must be called here.
    /// If a forwarding is missing in `AppChannel`, this test serves as a manual checklist
    /// to catch the gap during review.
    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn app_channel_forwards_all_channel_methods() {
        use zeph_core::channel::ToolOutputEvent;
        let mut ch = make_app_channel();
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
        // 8b. send_skill_catalog
        ch.send_skill_catalog(&[]).await.unwrap();
        // 9. send_thinking_chunk
        ch.send_thinking_chunk("...").await.unwrap();
        // 10. send_queue_count
        ch.send_queue_count(3).await.unwrap();
        // 11. send_usage
        ch.send_usage(10, 5, 8192, 0, 0, 0.0).await.unwrap();
        // 12. send_diff
        ch.send_diff(
            zeph_core::DiffData {
                file_path: String::new(),
                old_content: String::new(),
                new_content: String::new(),
            },
            "test-call-id",
        )
        .await
        .unwrap();
        // 13. send_tool_start
        ch.send_tool_start(ToolStartEvent {
            tool_name: ToolName::from("bash"),
            tool_call_id: "x".to_string(),
            params: None,
            parent_tool_use_id: None,
            started_at: std::time::Instant::now(),
            speculative: false,
            sandbox_profile: None,
            is_mcp: false,
        })
        .await
        .unwrap();
        // 14. send_tool_output
        ch.send_tool_output(ToolOutputEvent {
            tool_name: ToolName::from("bash"),
            display: "ok".to_string(),
            diff: None,
            filter_stats: None,
            kept_lines: None,
            locations: None,
            tool_call_id: "x".to_string(),
            is_error: false,
            terminal_id: None,
            parent_tool_use_id: None,
            raw_response: None,
            started_at: None,
        })
        .await
        .unwrap();
        // 15. send_stop_hint
        ch.send_stop_hint(StopHint::MaxTurnRequests).await.unwrap();
        // 16. confirm — skipped (reads from stdin; covered by separate test)
        // 17. notify_foreground_subagent_started
        ch.notify_foreground_subagent_started("sa-id", "planner")
            .await
            .unwrap();
        // 18. notify_foreground_subagent_completed
        ch.notify_foreground_subagent_completed("sa-id", "planner", true)
            .await
            .unwrap();
        // 19. send_context_estimate
        ch.send_context_estimate(4096).await.unwrap();
        // 20. elicit — skipped (CliChannel auto-declines on non-tty stdin regardless
        // of forwarding correctness; real dispatch is verified separately below via
        // TuiChannel, whose response only arrives if AppChannel forwards the call).
    }

    /// Regression test for #6388: `AppChannel::elicit` must forward to the
    /// wrapped channel's real implementation, not fall through to
    /// `Channel::elicit`'s default (auto-decline) method. `TuiChannel::elicit`
    /// only resolves once a response is sent down its `oneshot` channel, so if
    /// forwarding were missing this test would hang or observe `Declined`
    /// without ever receiving the `ElicitationRequest` event.
    #[tokio::test]
    async fn app_channel_forwards_elicit_to_real_implementation() {
        use zeph_core::channel::{ElicitationRequest, ElicitationResponse};
        use zeph_tui::{AgentEvent, TuiChannel};

        let (_user_tx, user_rx) = tokio::sync::mpsc::channel(1);
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel(1);
        let mut ch = AppChannel::Tui(TuiChannel::new(user_rx, agent_tx));

        let request = ElicitationRequest {
            server_name: "test-server".to_owned(),
            message: "provide input".to_owned(),
            fields: vec![],
        };
        let elicit_task = tokio::spawn(async move { ch.elicit(request).await });

        let event = agent_rx
            .recv()
            .await
            .expect("elicit() must forward an AgentEvent instead of auto-declining");
        let response_tx = match event {
            AgentEvent::ElicitationRequest { response_tx, .. } => response_tx,
            other => panic!("expected AgentEvent::ElicitationRequest, got {other:?}"),
        };
        response_tx
            .send(ElicitationResponse::Accepted(
                serde_json::json!({"ok": true}),
            ))
            .unwrap();

        let result = elicit_task.await.unwrap().unwrap();
        assert!(
            matches!(result, ElicitationResponse::Accepted(_)),
            "expected Accepted, got {result:?}"
        );
    }

    /// Regression test for #6388 follow-up gap: `AppChannel::send_context_estimate`
    /// must forward to `TuiChannel`'s real implementation (which emits an
    /// `AgentEvent::ContextEstimate`), not silently no-op via the trait default.
    #[tokio::test]
    async fn app_channel_forwards_send_context_estimate_to_real_implementation() {
        use zeph_tui::{AgentEvent, TuiChannel};

        let (_user_tx, user_rx) = tokio::sync::mpsc::channel(1);
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel(4);
        let mut ch = AppChannel::Tui(TuiChannel::new(user_rx, agent_tx));

        ch.send_context_estimate(4096).await.unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), agent_rx.recv())
            .await
            .expect(
                "send_context_estimate() must forward an AgentEvent instead of no-op'ing \
                 (timed out waiting for it)",
            )
            .expect("agent_tx channel closed unexpectedly");
        match event {
            AgentEvent::ContextEstimate(tokens) => assert_eq!(tokens, 4096),
            other => panic!("expected AgentEvent::ContextEstimate, got {other:?}"),
        }
    }

    /// Regression test for the mention-picker Skills tab (spec 084 S3): the
    /// `app_channel_forwards_all_channel_methods` checklist above only ever
    /// constructs `AppChannel::Standard(AnyChannel::Cli(..))`, whose `CliChannel`
    /// has no `send_skill_catalog` override — that call silently resolves to the
    /// trait's no-op default regardless of whether `AppChannel`'s own forwarding
    /// arm exists, so that test cannot detect the arm being deleted. This test
    /// exercises the `AppChannel::Tui` variant directly and asserts the catalog
    /// actually reaches `TuiChannel`'s `AgentEvent` channel.
    #[tokio::test]
    async fn app_channel_forwards_send_skill_catalog_to_real_implementation() {
        use zeph_core::channel::SkillCatalogItem;
        use zeph_tui::{AgentEvent, TuiChannel};

        let (_user_tx, user_rx) = tokio::sync::mpsc::channel(1);
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel(4);
        let mut ch = AppChannel::Tui(TuiChannel::new(user_rx, agent_tx));

        let items = vec![SkillCatalogItem {
            name: "web_search".to_owned(),
            description: "Search the web".to_owned(),
        }];
        ch.send_skill_catalog(&items).await.unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), agent_rx.recv())
            .await
            .expect(
                "send_skill_catalog() must forward an AgentEvent instead of no-op'ing \
                 (timed out waiting for it)",
            )
            .expect("agent_tx channel closed unexpectedly");
        match event {
            AgentEvent::SkillCatalog(got) => {
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].name, "web_search");
            }
            other => panic!("expected AgentEvent::SkillCatalog, got {other:?}"),
        }
    }
}

pub(crate) async fn build_cli_history(
    memory: &zeph_memory::semantic::SemanticMemory,
) -> Option<CliHistory> {
    let entries = memory
        .sqlite()
        .load_input_history(1000)
        .await
        .unwrap_or_default();
    let store = memory.sqlite().clone();
    let persist: Box<dyn Fn(&str) + Send> = Box::new(move |text: &str| {
        let store = store.clone();
        let text = text.to_owned();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = store.save_input_entry(&text).await {
                    tracing::warn!("failed to persist input history entry: {e}");
                }
            });
        }
    });
    Some((entries, persist))
}
