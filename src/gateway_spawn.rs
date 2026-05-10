// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Wraps an existing [`zeph_core::channel::Channel`] and merges in webhook payloads
/// arriving on a dedicated mpsc receiver.
///
/// All output methods (`send`, `send_chunk`, etc.) are forwarded to the inner channel
/// unchanged. Only the inbound path (`recv`, `try_recv`) also checks the webhook
/// receiver so the agent sees webhook payloads as regular `ChannelMessage`s.
#[cfg(feature = "gateway")]
pub(crate) struct GatewayChannel<C> {
    inner: C,
    webhook_rx: tokio::sync::mpsc::Receiver<zeph_core::ChannelMessage>,
}

#[cfg(feature = "gateway")]
impl<C> GatewayChannel<C> {
    /// Wrap `inner` and merge webhook messages from `webhook_rx`.
    pub(crate) fn new(
        inner: C,
        webhook_rx: tokio::sync::mpsc::Receiver<zeph_core::ChannelMessage>,
    ) -> Self {
        Self { inner, webhook_rx }
    }
}

#[cfg(feature = "gateway")]
impl<C: zeph_core::channel::Channel> zeph_core::channel::Channel for GatewayChannel<C> {
    async fn recv(
        &mut self,
    ) -> Result<Option<zeph_core::ChannelMessage>, zeph_core::channel::ChannelError> {
        tokio::select! {
            // Bias toward the inner channel (user input) so interactive sessions feel
            // responsive. biased = first branch wins when both are ready.
            biased;
            result = self.inner.recv() => result,
            msg = self.webhook_rx.recv() => Ok(msg),
        }
    }

    fn try_recv(&mut self) -> Option<zeph_core::ChannelMessage> {
        self.inner
            .try_recv()
            .or_else(|| self.webhook_rx.try_recv().ok())
    }

    fn supports_exit(&self) -> bool {
        self.inner.supports_exit()
    }

    async fn send(&mut self, text: &str) -> Result<(), zeph_core::channel::ChannelError> {
        self.inner.send(text).await
    }

    async fn send_chunk(&mut self, chunk: &str) -> Result<(), zeph_core::channel::ChannelError> {
        self.inner.send_chunk(chunk).await
    }

    async fn flush_chunks(&mut self) -> Result<(), zeph_core::channel::ChannelError> {
        self.inner.flush_chunks().await
    }

    async fn send_typing(&mut self) -> Result<(), zeph_core::channel::ChannelError> {
        self.inner.send_typing().await
    }

    async fn send_status(&mut self, text: &str) -> Result<(), zeph_core::channel::ChannelError> {
        self.inner.send_status(text).await
    }

    async fn send_thinking_chunk(
        &mut self,
        chunk: &str,
    ) -> Result<(), zeph_core::channel::ChannelError> {
        self.inner.send_thinking_chunk(chunk).await
    }

    async fn send_queue_count(
        &mut self,
        count: usize,
    ) -> Result<(), zeph_core::channel::ChannelError> {
        self.inner.send_queue_count(count).await
    }

    async fn send_usage(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        context_window: u64,
    ) -> Result<(), zeph_core::channel::ChannelError> {
        self.inner
            .send_usage(input_tokens, output_tokens, context_window)
            .await
    }

    async fn send_diff(
        &mut self,
        diff: zeph_core::DiffData,
        tool_call_id: &str,
    ) -> Result<(), zeph_core::channel::ChannelError> {
        self.inner.send_diff(diff, tool_call_id).await
    }

    async fn send_tool_start(
        &mut self,
        event: zeph_core::channel::ToolStartEvent,
    ) -> Result<(), zeph_core::channel::ChannelError> {
        self.inner.send_tool_start(event).await
    }

    async fn send_tool_output(
        &mut self,
        event: zeph_core::channel::ToolOutputEvent,
    ) -> Result<(), zeph_core::channel::ChannelError> {
        self.inner.send_tool_output(event).await
    }

    async fn confirm(&mut self, prompt: &str) -> Result<bool, zeph_core::channel::ChannelError> {
        self.inner.confirm(prompt).await
    }

    async fn elicit(
        &mut self,
        request: zeph_core::channel::ElicitationRequest,
    ) -> Result<zeph_core::channel::ElicitationResponse, zeph_core::channel::ChannelError> {
        self.inner.elicit(request).await
    }

    async fn send_stop_hint(
        &mut self,
        hint: zeph_core::channel::StopHint,
    ) -> Result<(), zeph_core::channel::ChannelError> {
        self.inner.send_stop_hint(hint).await
    }
}

#[cfg(feature = "gateway")]
pub(crate) fn spawn_gateway_server(
    config: &zeph_core::config::Config,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
    agent_input_tx: tokio::sync::mpsc::Sender<zeph_core::ChannelMessage>,
    #[cfg(feature = "prometheus")] metrics_registry: Option<(
        std::sync::Arc<prometheus_client::registry::Registry>,
        String,
    )>,
) {
    use zeph_gateway::GatewayServer;

    let (webhook_tx, mut webhook_rx) = tokio::sync::mpsc::channel::<String>(64);
    let gw = GatewayServer::new(
        &config.gateway.bind,
        config.gateway.port,
        webhook_tx,
        shutdown_rx,
    )
    .with_auth(config.gateway.auth_token.clone())
    .with_rate_limit(config.gateway.rate_limit)
    .with_max_body_size(config.gateway.max_body_size);

    #[cfg(feature = "prometheus")]
    let gw = if let Some((registry, path)) = metrics_registry {
        gw.with_metrics_registry(registry, path)
    } else {
        gw
    };

    tracing::info!(
        "Gateway server spawned on {}:{}",
        config.gateway.bind,
        config.gateway.port
    );

    tokio::spawn(async move {
        if let Err(e) = gw.serve().await {
            tracing::error!("gateway error: {e:#}");
        }
    });

    tokio::spawn(async move {
        while let Some(payload) = webhook_rx.recv().await {
            let msg = zeph_core::ChannelMessage {
                text: payload,
                attachments: vec![],
                is_guest_context: false,
                is_from_bot: false,
            };
            if agent_input_tx.send(msg).await.is_err() {
                tracing::debug!("gateway: agent input channel closed, stopping webhook forwarder");
                break;
            }
        }
    });
}

#[cfg(all(test, feature = "gateway"))]
mod tests {
    use super::*;
    use zeph_core::channel::Channel as _;
    use zeph_core::{ChannelMessage, LoopbackChannel};

    /// `GatewayChannel::try_recv` returns a webhook message when the inner channel
    /// has nothing queued — validates the merge path from fix #3500.
    #[test]
    fn try_recv_returns_webhook_message_when_inner_empty() {
        let (inner, _handle) = LoopbackChannel::pair(8);
        let (webhook_tx, webhook_rx) = tokio::sync::mpsc::channel::<ChannelMessage>(8);

        let mut ch = GatewayChannel::new(inner, webhook_rx);

        // No message yet — try_recv returns None.
        assert!(ch.try_recv().is_none(), "must be empty before any send");

        // Send a webhook payload.
        let msg = ChannelMessage {
            text: "hello from webhook".into(),
            attachments: vec![],
            is_guest_context: false,
            is_from_bot: false,
        };
        webhook_tx.try_send(msg).unwrap();

        // Now try_recv must surface the webhook message.
        let received = ch
            .try_recv()
            .expect("must receive the queued webhook message");
        assert_eq!(received.text, "hello from webhook");
    }

    /// `GatewayChannel::recv` resolves with a webhook message when the inner channel
    /// is closed and only the webhook receiver has a pending message.
    #[tokio::test]
    async fn recv_yields_webhook_message() {
        let (inner, _handle) = LoopbackChannel::pair(8);
        let (webhook_tx, webhook_rx) = tokio::sync::mpsc::channel::<ChannelMessage>(8);

        let mut ch = GatewayChannel::new(inner, webhook_rx);

        let msg = ChannelMessage {
            text: "webhook payload".into(),
            attachments: vec![],
            is_guest_context: false,
            is_from_bot: false,
        };
        webhook_tx.send(msg).await.unwrap();

        // recv() should return the webhook message.
        let result = ch.recv().await.expect("recv must not error");
        let received = result.expect("recv must return Some");
        assert_eq!(received.text, "webhook payload");
    }

    /// `GatewayChannel::supports_exit` delegates to the inner channel.
    #[test]
    fn supports_exit_delegates_to_inner() {
        let (inner, _handle) = LoopbackChannel::pair(8);
        let (_webhook_tx, webhook_rx) = tokio::sync::mpsc::channel::<ChannelMessage>(1);
        let ch = GatewayChannel::new(inner, webhook_rx);
        // LoopbackChannel::supports_exit returns false.
        assert!(!ch.supports_exit());
    }
}
