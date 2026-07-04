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
//! **`--acp` runs the ACP-HTTP transport in-process, on a second listener** (`[acp] http_bind`,
//! #5420): `zeph_acp::acp_router` already defines `/health` and `/sessions/{id}/messages` at its
//! root, which collide with serve's own `/health` and `/sessions*` on `.merge()` into one
//! `Router` — and the two `/sessions` surfaces are different data models (ACP's
//! `SqliteStore`-backed CRUD vs serve's `LiveSessionRegistry` + file event-logs) that cannot be
//! unified even in principle. So combined mode runs two foreground-joined (`tokio::join!`, not
//! `supervisor.spawn`ed) `axum::serve` listeners sharing ONE `TaskSupervisor`'s cancellation
//! token and ONE `SemanticMemory`/`SQLite` pool (see
//! [`run_serve_with_acp`]/`crate::acp::build_combined_deps`) rather than two independent pools
//! writing the same database file concurrently. ACP **stdio** (the standalone `--acp` command)
//! is not used here: `zeph_acp::serve_stdio` has no cancellation hook and reads immediate EOF
//! under a daemon's `StandardInput=null`, so it cannot be lifecycle-managed alongside a network
//! daemon — combined mode is HTTP-only for both transports. Requires the `acp-http` feature
//! (bundled in the `ide` feature bundle); without it, `--acp` is a hard error naming the
//! alternative (a separate `zeph --acp` process).

mod agent_factory;
pub(crate) mod deps;
mod handlers;
mod router;
#[cfg(test)]
mod test_support;

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
    /// Also run the ACP-HTTP protocol transport in-process, on `[acp] http_bind` (#5420).
    ///
    /// Distinct from the standalone `zeph --acp` (stdio) / `zeph --acp-http` commands: this
    /// flag runs ACP-over-HTTP as a *second listener* inside the same `serve-sessions` process,
    /// sharing its `SemanticMemory`/`SQLite` pool and `TaskSupervisor` — see the module doc.
    /// Requires the `acp-http` feature; without it, this is a hard error.
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
/// Dispatches to [`run_serve_with_acp`] when `args.acp` is set and the `acp-http` feature is
/// compiled in; otherwise serves the plain `/sessions*` HTTP/SSE API alone.
///
/// # Errors
///
/// Returns an error if the configured bind address is invalid, the port cannot be bound, the
/// HTTP server encounters a fatal I/O error after binding, or `--acp` is passed in a binary
/// compiled without the `acp-http` feature.
pub(crate) async fn handle_serve_sessions_command(
    args: ServeSessionsArgs,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use crate::bootstrap::{load_config_or_default, resolve_config_path};

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

    if args.acp {
        #[cfg(feature = "acp-http")]
        {
            return Box::pin(run_serve_with_acp(
                &args,
                config_path,
                http_addr,
                max_sessions,
            ))
            .await;
        }
        #[cfg(not(feature = "acp-http"))]
        {
            anyhow::bail!(
                "zeph serve-sessions --acp requires the `acp-http` feature (bundled in the \
                 `ide` feature bundle) — this binary was not compiled with it. Rebuild with \
                 `--features acp-http` (or `ide`), or run `zeph --acp` as a separate process \
                 alongside `zeph serve-sessions` instead."
            );
        }
    }

    let (deps, auth_token) = Box::pin(deps::build_serve_deps(
        config_path,
        args.vault_backend.as_deref(),
        args.vault_key.as_deref(),
        args.vault_path.as_deref(),
    ))
    .await?;

    check_require_auth_guard(serve_config, http_addr, auth_token.is_some())?;

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

    spawn_evict_task(&supervisor, &registry, serve_config.session_idle_ttl_secs);

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

/// Run `zeph serve-sessions --acp`: serve's `/sessions*` HTTP/SSE API and the ACP-HTTP
/// transport share ONE `SemanticMemory`/`SQLite` pool and ONE `TaskSupervisor`, built once via
/// [`crate::acp::build_combined_deps`] (#5420) — see the module doc for why this is two
/// listeners rather than one merged `Router` or ACP stdio.
///
/// # Errors
///
/// Returns an error if either bind address is invalid or already in use, a port clash is
/// detected between `[serve] http_addr` and `[acp] http_bind`, dependency construction fails,
/// or either HTTP server encounters a fatal I/O error.
#[cfg(feature = "acp-http")]
async fn run_serve_with_acp(
    args: &ServeSessionsArgs,
    config_path: Option<&std::path::Path>,
    http_addr: SocketAddr,
    max_sessions: usize,
) -> anyhow::Result<()> {
    let app = crate::bootstrap::AppBuilder::new(
        config_path,
        args.vault_backend.as_deref(),
        args.vault_key.as_deref(),
        args.vault_path.as_deref(),
    )
    .await?;
    let serve_config = app.config().serve.clone();
    let acp_bind_addr = app.config().acp.http_bind.clone();
    let acp_auth_token = app.config().acp.auth_token.clone();

    check_acp_http_port_clash(http_addr, &acp_bind_addr)?;

    let auth_token = deps::resolve_auth_token(&app).await;
    check_require_auth_guard(&serve_config, http_addr, auth_token.is_some())?;
    // M1-security (code review 2026-07-04): serve's own `require_auth` guard only covers
    // `http_addr`. Without an equivalent check here, `[serve] require_auth = true` gives an
    // operator false confidence that the whole combined process is authenticated, while the
    // ACP listener — sharing the same `acp_sessions` table serve's guard protects — could be
    // reachable non-loopback with no token at all.
    check_acp_auth_guard(&acp_bind_addr, acp_auth_token.is_some())?;

    let cancel = tokio_util::sync::CancellationToken::new();
    let supervisor = Arc::new(TaskSupervisor::new(cancel.clone()));
    let (serve_deps, acp_deps, _acp_keepalive) =
        Box::pin(crate::acp::build_combined_deps(&app, &supervisor)).await?;

    let registry = Arc::new(LiveSessionRegistry::new());
    let memory_sqlite = serve_deps.memory.sqlite().clone();
    let sanitizer = zeph_core::ContentSanitizer::new(&app.config().security.content_isolation);
    let state = AppState {
        registry: Arc::clone(&registry),
        started_at: std::time::Instant::now(),
        supervisor: TaskSupervisor::clone(&supervisor),
        deps: serve_deps,
        mailbox_capacity: serve_config.max_queued_prompts,
        max_sessions,
        sanitizer,
    };

    spawn_evict_task(&supervisor, &registry, serve_config.session_idle_ttl_secs);

    let mut acp_deps = acp_deps;
    let acp_server_config = crate::acp::acp_http_server_config(&mut acp_deps);
    let spawner = crate::acp::acp_http_ready_spawner(Arc::new(acp_deps)).await;
    let acp_http_state =
        zeph_acp::AcpHttpState::new(spawner, acp_server_config).with_store(memory_sqlite);
    acp_http_state.mark_ready();
    // N2 (critic round 2): `start_reaper` spawns its own infinite `interval.tick()` loop on
    // `AcpHttpState`'s PRIVATE supervisor (constructed inside `AcpHttpState::new`) — not
    // `supervisor` above, and NOT self-terminating. Left unsupervised-by-us is still safe: it
    // holds no resource needing graceful drain (an in-memory `DashMap::retain`) and dies at
    // process exit; routing it through the shared `supervisor` would need an `AcpHttpState` API
    // change and is deliberately deferred as follow-up scope, not part of #5420.
    acp_http_state.start_reaper();
    let acp_router = zeph_acp::acp_router(acp_http_state);

    let http_listener = tokio::net::TcpListener::bind(http_addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {http_addr}: {e}"))?;
    tracing::info!(addr = %http_addr, max_sessions, "zeph serve-sessions listening");
    let acp_listener = tokio::net::TcpListener::bind(&acp_bind_addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind ACP HTTP {acp_bind_addr}: {e}"))?;
    tracing::info!(addr = %acp_bind_addr, "zeph serve-sessions: ACP HTTP transport listening");

    let http_router = router::build_router(state, auth_token.as_deref(), serve_config.require_auth);

    // N3 (critic round 2): exactly one SIGTERM/Ctrl-C -> cancel producer, shared by both
    // servers' graceful shutdown. Embedding the signal-wait inside both `.with_graceful_shutdown`
    // closures (the single-server pattern above) would mean neither one actually drives the
    // other's shutdown — each would independently wait on its own signal instead of one firing
    // `cancel` for both to observe via `cancel.cancelled()`.
    let shutdown_cancel = cancel.clone();
    let shutdown_producer = async move {
        wait_for_shutdown_signal().await;
        tracing::info!("zeph serve-sessions: shutdown signal received");
        shutdown_cancel.cancel();
    };
    // S1 (code review 2026-07-04): `tokio::join!` runs every future to completion and never
    // cancels early — without the `.cancel()` call on each future's own completion (`Ok` or
    // `Err`), a fatal error on one listener would leave `join!` blocked on `shutdown_producer`
    // (awaiting a signal that may never come) while the *other* listener keeps silently
    // accepting connections, and the error would only surface after an external SIGTERM/Ctrl-C.
    // Cancelling here converges any fatal exit on the one shared token immediately, draining the
    // sibling listener and surfacing the error via the `?`s below without waiting on anything
    // external. `CancellationToken::cancel()` is idempotent, so this is safe to call redundantly
    // alongside `shutdown_producer`'s own cancel on a graceful signal-driven shutdown.
    let http_shutdown_cancel = cancel.clone();
    let http_done_cancel = cancel.clone();
    let serve_http = async move {
        let result = axum::serve(
            http_listener,
            http_router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { http_shutdown_cancel.cancelled().await })
        .await;
        http_done_cancel.cancel();
        result
    };
    let acp_shutdown_cancel = cancel.clone();
    let acp_done_cancel = cancel.clone();
    let serve_acp = async move {
        let result = axum::serve(acp_listener, acp_router)
            .with_graceful_shutdown(async move { acp_shutdown_cancel.cancelled().await })
            .await;
        acp_done_cancel.cancel();
        result
    };

    let ((), http_result, acp_result) = tokio::join!(shutdown_producer, serve_http, serve_acp);
    http_result.map_err(|e| anyhow::anyhow!("http server error: {e}"))?;
    acp_result.map_err(|e| anyhow::anyhow!("ACP http server error: {e}"))?;

    supervisor.shutdown_all(Duration::from_secs(30)).await;
    tracing::info!("zeph serve-sessions: shutdown complete (combined ACP-HTTP mode)");
    Ok(())
}

/// `/sessions*` endpoints can execute shell/file/web tools on behalf of any caller that reaches
/// this port. If `require_auth` is set but no token could be resolved from the vault,
/// `auth_middleware` would reject every single request (`AuthConfig::new(None, true)`) — bind
/// anyway on loopback (still useful for local-only access without a token requirement in
/// practice), but refuse a non-loopback bind rather than silently serving an API nobody can ever
/// successfully call, or worse, one where the operator assumes auth is protecting it.
fn check_require_auth_guard(
    serve_config: &zeph_config::ServeConfig,
    http_addr: SocketAddr,
    has_auth_token: bool,
) -> anyhow::Result<()> {
    if serve_config.require_auth && !has_auth_token && !http_addr.ip().is_loopback() {
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
    Ok(())
}

/// Mirrors [`check_require_auth_guard`] for the ACP-HTTP listener (M1-security, code review
/// 2026-07-04). `[acp] auth_token` has no `require_auth` on/off toggle the way `[serve]` does —
/// an unset token always means unauthenticated, so refusing a non-loopback bind is the correct
/// parallel to serve's `require_auth = true` path (there is no explicit "I know it's insecure,
/// disable the check" escape hatch to mirror for the warn-only branch). Without this guard,
/// `[serve] require_auth = true` gives an operator false confidence that the whole combined
/// process is authenticated, when the ACP listener — sharing the same `acp_sessions` table
/// serve's guard protects — could be reachable non-loopback with no token at all.
///
/// If `acp_http_bind` doesn't parse to a `SocketAddr` (e.g. a bare hostname), the check is
/// skipped and the real bind call surfaces any failure instead of a false pre-check rejection —
/// same tolerance as [`check_acp_http_port_clash`].
#[cfg(feature = "acp-http")]
fn check_acp_auth_guard(acp_http_bind: &str, has_acp_auth_token: bool) -> anyhow::Result<()> {
    let Ok(acp_addr) = acp_http_bind.parse::<SocketAddr>() else {
        return Ok(());
    };
    if !has_acp_auth_token && !acp_addr.ip().is_loopback() {
        anyhow::bail!(
            "refusing to bind ACP HTTP {acp_addr}: [acp] auth_token is not set — the ACP \
             listener would be reachable over the network with no authentication, sharing the \
             same acp_sessions table [serve] require_auth is meant to protect. Set \
             [acp] auth_token, or bind [acp] http_bind to a loopback address (127.0.0.1 or ::1)."
        );
    }
    Ok(())
}

/// N1 (critic round 2): wildcard-aware port-clash pre-check between `[serve] http_addr` and
/// `[acp] http_bind`. Hard-errors when ports match AND either the IPs are equal or either IP is
/// unspecified (`0.0.0.0`/`::`, which covers every local address including loopback) — the same
/// port on two genuinely distinct concrete IPs is legal (logged at `warn!`, since it is unusual
/// but not wrong). If `acp_http_bind` doesn't parse to a `SocketAddr` (e.g. a bare hostname), the
/// check is skipped and the real bind call surfaces `EADDRINUSE` instead of a false pre-check
/// failure.
///
/// Cross-IP-family unspecified-vs-concrete combinations (e.g. `0.0.0.0:P` vs `[::1]:P`) are
/// intentionally treated as a clash even though some platforms (with `bindv6only` disabled,
/// e.g.) would successfully bind both — safe-by-default over precise dual-stack modeling
/// (impl-critic M1).
#[cfg(feature = "acp-http")]
fn check_acp_http_port_clash(http_addr: SocketAddr, acp_http_bind: &str) -> anyhow::Result<()> {
    let Ok(acp_addr) = acp_http_bind.parse::<SocketAddr>() else {
        return Ok(());
    };
    let same_port = http_addr.port() == acp_addr.port();
    let ips_overlap = http_addr.ip() == acp_addr.ip()
        || http_addr.ip().is_unspecified()
        || acp_addr.ip().is_unspecified();
    if same_port && ips_overlap {
        anyhow::bail!(
            "port clash: [serve] http_addr ({http_addr}) and [acp] http_bind ({acp_addr}) would \
             bind overlapping addresses on the same port. Set them to different ports, or bind \
             each to a distinct concrete IP."
        );
    }
    if same_port {
        tracing::warn!(
            serve_addr = %http_addr,
            acp_addr = %acp_addr,
            "[serve] http_addr and [acp] http_bind share the same port on distinct concrete \
             IPs — legal, but double-check this is intentional"
        );
    }
    Ok(())
}

#[cfg(all(test, feature = "acp-http"))]
mod guard_tests {
    use super::{check_acp_auth_guard, check_acp_http_port_clash};

    fn addr(s: &str) -> std::net::SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn same_port_same_ip_is_a_clash() {
        let result = check_acp_http_port_clash(addr("127.0.0.1:8080"), "127.0.0.1:8080");
        assert!(result.is_err(), "identical addr:port must be rejected");
    }

    #[test]
    fn same_port_wildcard_vs_concrete_is_a_clash() {
        let result = check_acp_http_port_clash(addr("0.0.0.0:8080"), "127.0.0.1:8080");
        assert!(
            result.is_err(),
            "an unspecified IP on one side covers every concrete address on that port"
        );
    }

    #[test]
    fn same_port_distinct_concrete_ips_is_legal() {
        let result = check_acp_http_port_clash(addr("127.0.0.1:8080"), "10.0.0.5:8080");
        assert!(
            result.is_ok(),
            "same port on two genuinely distinct concrete IPs must not be rejected"
        );
    }

    #[test]
    fn unparseable_acp_bind_skips_the_check() {
        let result = check_acp_http_port_clash(addr("127.0.0.1:8080"), "not-a-valid-socket-addr");
        assert!(
            result.is_ok(),
            "a bare hostname must be tolerated here; the real bind call surfaces any failure"
        );
    }

    #[test]
    fn non_loopback_bind_without_token_is_refused() {
        let result = check_acp_auth_guard("0.0.0.0:9800", false);
        assert!(
            result.is_err(),
            "a non-loopback ACP bind with no auth token must be refused"
        );
    }

    #[test]
    fn non_loopback_bind_with_token_is_allowed() {
        let result = check_acp_auth_guard("0.0.0.0:9800", true);
        assert!(result.is_ok(), "a configured auth token permits any bind");
    }

    #[test]
    fn loopback_bind_without_token_is_allowed() {
        let result = check_acp_auth_guard("127.0.0.1:9800", false);
        assert!(
            result.is_ok(),
            "loopback-only exposure without a token is the same trade-off serve's own guard allows"
        );
    }

    #[test]
    fn unparseable_acp_bind_skips_the_auth_check() {
        let result = check_acp_auth_guard("not-a-valid-socket-addr", false);
        assert!(
            result.is_ok(),
            "a bare hostname must be tolerated here; the real bind call surfaces any failure"
        );
    }
}

/// Registers `serve.evict` (spec §9.3) on `supervisor` — shared by both the plain and combined
/// (`--acp`) startup paths.
fn spawn_evict_task(
    supervisor: &TaskSupervisor,
    registry: &Arc<LiveSessionRegistry>,
    ttl_secs: u64,
) {
    let idle_ttl = Duration::from_secs(ttl_secs);
    let evict_registry = Arc::clone(registry);
    let evict_cancel = supervisor.cancellation_token();
    supervisor.spawn(TaskDescriptor {
        name: "serve.evict",
        restart: RestartPolicy::Restart {
            max: 5,
            base_delay: Duration::from_secs(1),
        },
        factory: move || evict_loop(Arc::clone(&evict_registry), idle_ttl, evict_cancel.clone()),
    });
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
