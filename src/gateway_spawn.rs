// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(feature = "gateway")]
pub(crate) fn spawn_gateway_server(
    config: &zeph_core::config::Config,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
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

    // Drain incoming webhooks — full agent loopback wiring tracked in #1026.
    tokio::spawn(async move {
        while let Some(payload) = webhook_rx.recv().await {
            tracing::debug!(
                bytes = payload.len(),
                "webhook received (loopback not wired)"
            );
        }
    });
}
