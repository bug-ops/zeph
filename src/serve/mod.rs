// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `zeph serve-sessions` — persistent agent service exposing sessions over HTTP/SSE
//! (spec-068 §9, #5343).
//!
//! Named `serve-sessions` rather than the spec's literal `zeph serve`: `Command::Serve` already
//! names the scheduler's foreground daemon (`#[cfg(all(unix, feature = "scheduler"))]`,
//! `src/cli.rs`), and both features can be enabled simultaneously, so a second command claiming
//! the same top-level name is not viable.
//!
//! **Status**: process lifecycle (bind, listen, graceful shutdown on SIGTERM/Ctrl-C via
//! `TaskSupervisor::shutdown_all`), the unauthenticated `/health` endpoint, the `serve.evict`
//! idle-eviction task (spec §9.3), and the full `/sessions*` surface (spec §9.4 —
//! create/list/get/delete/prompt/events/fork) are implemented. `[serve] require_auth` /
//! `auth_token_vault_key` are wired: every `/sessions*` route (not `/health`) is behind
//! `zeph_common::http_middleware::auth_middleware`, keyed off a bearer token resolved from the
//! vault at startup. If `require_auth = true` (the default) but no token could be resolved,
//! [`handle_serve_sessions_command`] refuses to bind a non-loopback address — otherwise the API
//! would be reachable over the network while rejecting every single request, or worse, an
//! operator could assume auth is protecting it when it silently isn't.
//!
//! **`--acp` is intentionally not implemented as in-process combination**: `src/acp.rs`'s
//! `run_acp_server`/`run_acp_http_server` each build a complete, independent `SharedAgentDeps`
//! (own `SemanticMemory`/`SQLite` pool, provider, `McpManager`, skill registry,
//! `TaskSupervisor`) with no path to share those with [`deps::ServeAgentDeps`]. Running both in
//! one process would mean two independent `SQLite` connection pools writing the same database
//! file concurrently — a real contention/correctness risk, not just wasted resources — plus
//! duplicate MCP subprocess spawning if MCP servers are configured. `--acp` therefore returns an
//! error naming the correct alternative (run `zeph --acp` or `zeph --acp-http` as a *separate*
//! process alongside `zeph serve-sessions`) rather than silently building something unsafe.
//! Proper in-process support would need `build_acp_deps` refactored to accept prebuilt
//! shared resources (it already does this for MCP via `prebuilt_mcp_manager`) — real design work
//! deserving its own attention, not something to squeeze into this change.

mod agent_factory;
mod deps;
mod handlers;
mod router;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use zeph_common::task_supervisor::{RestartPolicy, TaskDescriptor, TaskSupervisor};
use zeph_core::serve::LiveSessionRegistry;

use self::deps::ServeAgentDeps;

/// How often `serve.evict` scans [`LiveSessionRegistry::idle_candidates`].
const EVICT_SCAN_INTERVAL: Duration = Duration::from_mins(1);

/// CLI-supplied overrides for `zeph serve-sessions` (spec-068 §9, #5343).
pub(crate) struct ServeSessionsArgs {
    /// Overrides `[serve] http_addr` when set.
    pub(crate) http_addr: Option<String>,
    /// Also run the ACP protocol transport alongside the HTTP/SSE API.
    ///
    /// Not implemented as in-process combination — see the module doc. Passing this flag is a
    /// hard error naming the correct alternative (a separate `zeph --acp`/`zeph --acp-http`
    /// process) rather than a silent no-op, since a user passing `--acp` clearly expects it to
    /// take effect.
    pub(crate) acp: bool,
    /// Overrides `[serve] max_sessions` when set.
    pub(crate) max_sessions: Option<usize>,
    /// `--vault` — secrets backend override (`"env"` or `"age"`).
    pub(crate) vault_backend: Option<String>,
    /// `--vault-key` — path to the age identity file.
    pub(crate) vault_key: Option<std::path::PathBuf>,
    /// `--vault-path` — path to the age-encrypted secrets file.
    pub(crate) vault_path: Option<std::path::PathBuf>,
}

/// Shared HTTP handler state.
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) registry: Arc<LiveSessionRegistry>,
    pub(crate) started_at: std::time::Instant,
    pub(crate) supervisor: TaskSupervisor,
    pub(crate) deps: ServeAgentDeps,
    pub(crate) mailbox_capacity: usize,
    pub(crate) max_sessions: usize,
    /// Sanitizes `POST /sessions/:id/prompt` bodies as `ExternalUntrusted` before they reach the
    /// agent loopback queue — a valid bearer token proves the caller knows the shared secret, not
    /// that the prompt content is safe (#5474). Same `ExternalUntrusted` tier as the gateway's
    /// `forward_webhooks` (`ContentSourceKind::ChannelMessage`, `src/gateway_spawn.rs`) and A2A's
    /// `AgentTaskProcessor` (`ContentSourceKind::A2aMessage`, `src/daemon.rs`) — this handler uses
    /// `ChannelMessage`, matching the gateway's kind, not A2A's.
    pub(crate) sanitizer: zeph_core::ContentSanitizer,
}

/// Run `zeph serve-sessions` until a shutdown signal (SIGTERM/Ctrl-C) is received.
///
/// # Errors
///
/// Returns an error if the configured bind address is invalid, the port cannot be bound, or the
/// HTTP server encounters a fatal I/O error after binding.
pub(crate) async fn handle_serve_sessions_command(
    args: ServeSessionsArgs,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use crate::bootstrap::{load_config_or_default, resolve_config_path};

    if args.acp {
        anyhow::bail!(
            "zeph serve-sessions --acp does not run the ACP transport in-process: doing so \
             would mean two independent SQLite connection pools writing the same database file \
             concurrently (build_acp_deps builds its own complete SharedAgentDeps with no path \
             to share resources with serve-sessions' own agent dependencies), plus duplicate \
             MCP subprocess spawning if MCP servers are configured. Run `zeph --acp` or \
             `zeph --acp-http` as a separate process alongside `zeph serve-sessions` instead."
        );
    }

    let config_file = resolve_config_path(config_path);
    let config = load_config_or_default(&config_file);
    let serve_config = &config.serve;

    let http_addr: SocketAddr = args
        .http_addr
        .as_deref()
        .unwrap_or(&serve_config.http_addr)
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid [serve] http_addr: {e}"))?;
    let max_sessions = args.max_sessions.unwrap_or(serve_config.max_sessions);

    let (deps, auth_token) = Box::pin(deps::build_serve_deps(
        config_path,
        args.vault_backend.as_deref(),
        args.vault_key.as_deref(),
        args.vault_path.as_deref(),
    ))
    .await?;

    // `/sessions*` endpoints can execute shell/file/web tools on behalf of any caller that
    // reaches this port. If `require_auth` is set but no token could be resolved from the vault,
    // `auth_middleware` would reject every single request (`AuthConfig::new(None, true)`) — bind
    // anyway on loopback (still useful for local-only access without a token requirement in
    // practice), but refuse a non-loopback bind rather than silently serving an API nobody can
    // ever successfully call, or worse, one where the operator assumes auth is protecting it.
    if serve_config.require_auth && auth_token.is_none() && !http_addr.ip().is_loopback() {
        anyhow::bail!(
            "refusing to bind {http_addr}: [serve] require_auth is true but no auth token was \
             resolved from the vault key \"{}\" — every request would be rejected. Set that \
             vault key, bind to a loopback address (127.0.0.1 or ::1), or set \
             [serve] require_auth = false to disable authentication explicitly.",
            serve_config.auth_token_vault_key
        );
    }
    if !serve_config.require_auth {
        tracing::warn!(
            "[serve] require_auth is false — /sessions* endpoints are unauthenticated; only \
             bind to loopback or a trusted network"
        );
    }

    let cancel = tokio_util::sync::CancellationToken::new();
    let supervisor = TaskSupervisor::new(cancel.clone());
    let registry = Arc::new(LiveSessionRegistry::new());
    let state = AppState {
        registry: Arc::clone(&registry),
        started_at: std::time::Instant::now(),
        supervisor: supervisor.clone(),
        deps,
        mailbox_capacity: serve_config.max_queued_prompts,
        max_sessions,
        sanitizer: zeph_core::ContentSanitizer::new(&config.security.content_isolation),
    };

    let idle_ttl = Duration::from_secs(serve_config.session_idle_ttl_secs);
    let evict_registry = Arc::clone(&registry);
    let evict_cancel = supervisor.cancellation_token();
    supervisor.spawn(TaskDescriptor {
        name: "serve.evict",
        restart: RestartPolicy::Restart {
            max: 5,
            base_delay: Duration::from_secs(1),
        },
        factory: move || evict_loop(Arc::clone(&evict_registry), idle_ttl, evict_cancel.clone()),
    });

    let listener = tokio::net::TcpListener::bind(http_addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {http_addr}: {e}"))?;
    tracing::info!(addr = %http_addr, max_sessions, "zeph serve-sessions listening");

    let router = router::build_router(state, auth_token.as_deref(), serve_config.require_auth);
    let shutdown_cancel = cancel.clone();
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        wait_for_shutdown_signal().await;
        tracing::info!("zeph serve-sessions: shutdown signal received");
        shutdown_cancel.cancel();
    })
    .await
    .map_err(|e| anyhow::anyhow!("http server error: {e}"))?;

    supervisor.shutdown_all(Duration::from_secs(30)).await;
    tracing::info!("zeph serve-sessions: shutdown complete");
    Ok(())
}

/// `serve.evict` (spec §9.3): periodically scans `registry` for idle candidates (no attached
/// broadcast subscribers, `last_active` older than `ttl`) and cancels each one's own
/// [`zeph_core::serve::SessionActorHandle::cancel`] token — the same mechanism a caller would use
/// to end one specific session gracefully, distinct from process-wide shutdown.
///
/// Uses `registry.remove` (not `registry.get`, which refreshes `last_active`) so an eviction scan
/// itself never resets the idle timer it is reading. There is a small window between
/// `idle_candidates` returning a snapshot and this loop iterating it where a session could
/// legitimately regain an active subscriber — evicting it anyway in that rare case is a
/// false-positive disconnect, not a correctness issue: the client's next connection replays the
/// durable JSONL log from where it left off (spec §9.3's own resume-by-replay design already
/// covers this).
async fn evict_loop(
    registry: Arc<LiveSessionRegistry>,
    ttl: Duration,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut ticker = tokio::time::interval(EVICT_SCAN_INTERVAL);
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::debug!("serve.evict: shutting down");
                return;
            }
            _ = ticker.tick() => {
                for id in registry.idle_candidates(ttl) {
                    if let Some(handle) = registry.remove(&id) {
                        tracing::info!(session_id = %id.as_str(), "serve.evict: evicting idle session");
                        handle.cancel.cancel();
                    }
                }
            }
        }
    }
}

/// Waits for SIGTERM (Unix) or Ctrl-C, whichever arrives first.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            tracing::warn!("failed to install SIGTERM handler; only Ctrl-C will trigger shutdown");
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
