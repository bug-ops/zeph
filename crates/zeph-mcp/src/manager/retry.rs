// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Connection retry, exponential backoff, and the low-level per-server connect path.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use dashmap::DashMap;
use tokio::sync::mpsc;

use crate::client::{EnvPolicy, McpClient, StderrPolicy, ToolRefreshEvent};
use crate::error::McpError;

use super::{McpTransport, ServerEntry, StatusTx};

/// Compute the sleep duration before retry attempt `attempt + 1`.
///
/// Doubling exponential backoff capped at 8 s, with up to -25%/+0% jitter applied to the
/// capped value so that concurrent servers do not retry in lock-step:
/// `jitter(min(base_ms * 2^(attempt - 1), 8_000ms))`.
///
/// The jitter range is `[nominal * 3/4, nominal]` (full-jitter, AWS-style), so the
/// returned duration is always ≤ the nominal (capped) backoff.
///
/// For `base_ms = 1000, max_connect_attempts = 3` the sequence is **1 s, 2 s**
/// (three attempts → two inter-attempt gaps), each with up to -25% variance.
/// For `max_connect_attempts = 10` the nominal value caps at 8 s after the 4th
/// inter-attempt gap; jitter keeps the actual sleep in [6 s, 8 s].
///
/// `attempt` is 1-based and corresponds to the just-failed attempt index.
pub(super) fn connect_retry_backoff(attempt: u8, base_ms: u64) -> Duration {
    use rand::RngExt as _;
    const CAP_MS: u64 = 8_000;
    let exp = u32::from(attempt.saturating_sub(1));
    let nominal = base_ms
        .saturating_mul(2u64.saturating_pow(exp.min(20)))
        .min(CAP_MS);
    let low = nominal * 3 / 4;
    let jittered = if low < nominal {
        rand::rng().random_range(low..=nominal)
    } else {
        nominal
    };
    Duration::from_millis(jittered)
}

/// Classify whether a connection error is transient and worth retrying.
///
/// Every [`McpError`] variant must be explicitly listed so that adding a new variant
/// triggers a compile error that forces deliberate classification.
pub(super) fn is_retryable_connect_error(err: &McpError) -> bool {
    match err {
        // Transient transport / handshake failures.
        McpError::Connection { .. } | McpError::Timeout { .. } => true,
        // Permanent / structural failures — never retry.
        McpError::ManagerShuttingDown { .. }
        | McpError::CommandNotAllowed { .. }
        | McpError::EnvVarBlocked { .. }
        | McpError::SsrfBlocked { .. }
        | McpError::InvalidUrl { .. }
        | McpError::PolicyViolation(_)
        | McpError::OAuthError { .. }
        | McpError::OAuthCallbackTimeout { .. }
        | McpError::ServerNotFound { .. }
        | McpError::ServerAlreadyConnected { .. }
        | McpError::ToolListLocked { .. }
        | McpError::ToolCall { .. }
        | McpError::ToolNotFound { .. }
        | McpError::Qdrant(_)
        | McpError::Json(_)
        | McpError::IntConversion(_)
        | McpError::Embedding(_)
        | McpError::HttpAuth { .. } => false,
    }
}

/// Retry an async `attempt_fn` up to `max_attempts` times with exponential backoff.
///
/// Status messages are emitted via `status_tx` (when present):
/// - Attempt 1: `"Connecting to MCP server {server_id}..."`
/// - Attempts 2..max: `"Reconnecting to MCP server {server_id} (attempt {n}/{max})..."`
///
/// Cancellation via `shutdown` is checked before every attempt and during backoff sleeps.
/// On cancellation, returns `Err(McpError::ManagerShuttingDown)` immediately.
///
/// `retry_backoff_base_ms` is the base delay in milliseconds; the actual delay doubles
/// with each attempt, capped at 8 000 ms. See [`connect_retry_backoff`] for details.
#[tracing::instrument(name = "mcp.manager.retry_loop", skip_all, fields(server_id = %server_id, max_attempts), err)]
pub(super) async fn retry_loop<F, Fut>(
    server_id: &str,
    max_attempts: u8,
    retry_backoff_base_ms: u64,
    status_tx: Option<&StatusTx>,
    shutdown: &CancellationToken,
    mut attempt_fn: F,
) -> Result<McpClient, McpError>
where
    F: FnMut(u8) -> Fut,
    Fut: std::future::Future<Output = Result<McpClient, McpError>>,
{
    let mut last_err = McpError::ManagerShuttingDown {
        server_id: server_id.to_owned(),
    };

    for attempt in 1..=max_attempts {
        // Pre-attempt cancellation check.
        if shutdown.is_cancelled() {
            return Err(McpError::ManagerShuttingDown {
                server_id: server_id.to_owned(),
            });
        }

        // Emit status message.
        if let Some(stx) = status_tx {
            // SECURITY: only server_id is included — never transport URL, headers, or tokens.
            let msg = if attempt == 1 {
                format!("Connecting to MCP server {server_id}...")
            } else {
                format!(
                    "Reconnecting to MCP server {server_id} (attempt {attempt}/{max_attempts})..."
                )
            };
            let _ = stx.send(msg);
        }

        match attempt_fn(attempt).await {
            Ok(client) => return Ok(client),
            Err(e) => {
                let retryable = is_retryable_connect_error(&e);
                tracing::warn!(
                    server_id,
                    attempt,
                    max_attempts,
                    retryable,
                    error = %e,
                    "MCP server connection attempt failed"
                );
                last_err = e;
                if !retryable || attempt == max_attempts {
                    break;
                }
                // Cancellable backoff sleep.
                let delay = connect_retry_backoff(attempt, retry_backoff_base_ms);
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => {
                        return Err(McpError::ManagerShuttingDown {
                            server_id: server_id.to_owned(),
                        });
                    }
                    () = tokio::time::sleep(delay) => {}
                }
            }
        }
    }

    Err(last_err)
}

/// Connect to a single MCP server with startup retry and exponential backoff.
///
/// This is a thin shim over [`retry_loop`] that binds [`connect_entry`] as the attempt
/// function. `add_server` uses [`connect_entry`] directly (single-attempt, no retry).
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(name = "mcp.manager.connect_with_retry", skip_all, fields(server_id = %entry.id), err)]
pub(super) async fn connect_with_retry(
    entry: &ServerEntry,
    allowed_commands: &[String],
    suppress_stderr: bool,
    tx: mpsc::Sender<ToolRefreshEvent>,
    last_refresh: Arc<DashMap<String, Instant>>,
    handler_cfg: &crate::client::HandlerConfig,
    max_attempts: u8,
    retry_backoff_base_ms: u64,
    status_tx: Option<&StatusTx>,
    shutdown: &CancellationToken,
) -> Result<McpClient, McpError> {
    retry_loop(
        entry.id.as_str(),
        max_attempts,
        retry_backoff_base_ms,
        status_tx,
        shutdown,
        |_attempt| {
            let tx = tx.clone();
            let last_refresh = Arc::clone(&last_refresh);
            async move {
                connect_entry(
                    entry,
                    allowed_commands,
                    suppress_stderr,
                    tx,
                    last_refresh,
                    handler_cfg,
                )
                .await
            }
        },
    )
    .await
}

#[allow(clippy::too_many_arguments)]
// function with many required inputs; a *Params struct would be more verbose without simplifying the call site
#[tracing::instrument(name = "mcp.manager.connect_entry", skip_all, fields(server_id = %entry.id), err)]
pub(super) async fn connect_entry(
    entry: &ServerEntry,
    allowed_commands: &[String],
    suppress_stderr: bool,
    tx: mpsc::Sender<ToolRefreshEvent>,
    last_refresh: Arc<DashMap<String, Instant>>,
    handler_cfg: &crate::client::HandlerConfig,
) -> Result<McpClient, McpError> {
    match &entry.transport {
        McpTransport::Stdio { command, args, env } => {
            let stderr_policy = if suppress_stderr {
                StderrPolicy::Suppress
            } else {
                StderrPolicy::Forward
            };
            let env_policy = if entry.env_isolation {
                EnvPolicy::Isolated
            } else {
                EnvPolicy::InheritAll
            };
            McpClient::connect(
                &entry.id,
                command,
                args,
                env,
                allowed_commands,
                entry.timeout,
                stderr_policy,
                env_policy,
                tx,
                last_refresh,
                handler_cfg.clone(),
            )
            .await
        }
        McpTransport::Http { url, headers } => {
            if headers.is_empty() {
                McpClient::connect_url(
                    &entry.id,
                    url,
                    entry.timeout,
                    entry.trust_level,
                    tx,
                    last_refresh,
                    handler_cfg.clone(),
                )
                .await
            } else {
                McpClient::connect_url_with_headers(
                    &entry.id,
                    url,
                    headers,
                    entry.timeout,
                    entry.trust_level,
                    tx,
                    last_refresh,
                    handler_cfg.clone(),
                )
                .await
            }
        }
        McpTransport::OAuth { .. } => {
            // OAuth connections are handled separately in connect_oauth_deferred().
            Err(McpError::OAuthError {
                server_id: entry.id.clone(),
                message: "OAuth transport cannot be used via connect_entry".into(),
            })
        }
    }
}
