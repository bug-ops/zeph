// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! ACP sub-agent client.
//!
//! This module lets Zeph act as a **client** that spawns an external ACP-compatible
//! agent subprocess and communicates with it using the Agent Client Protocol. It is
//! the inverse of `zeph-acp`'s server role: rather than accepting connections from
//! IDEs, this client *initiates* a connection to a child process.
//!
//! # Architecture
//!
//! ```text
//! SubagentHandle  ──cmd_tx──►  driver task  ──JSON-RPC──►  child process
//!      │                            │                           │
//!      │◄──ready_rx─────────────────┘                           │
//!      │                                                        │
//!      └──send_prompt / read_update / close                     │
//! ```
//!
//! The driver runs inside `Client.builder().connect_with(...)` and serialises all
//! ACP operations through a command channel. Callers interact only with
//! [`SubagentHandle`].
//!
//! # Quick start
//!
//! ```no_run
//! use zeph_acp::client::{SubagentConfig, spawn_subagent};
//!
//! # async fn example() -> Result<(), zeph_acp::client::AcpClientError> {
//! let cfg = SubagentConfig {
//!     command: "cargo run --quiet -- --acp".to_owned(),
//!     auto_approve_permissions: true,
//!     ..SubagentConfig::default()
//! };
//!
//! let outcome = run_session(cfg, "hello").await?;
//! println!("{}", outcome.text);
//! # Ok(())
//! # }
//!
//! # use zeph_acp::client::run_session;
//! ```

pub mod config;
pub mod error;

pub(crate) mod driver;
pub(crate) mod transport;

pub use config::{AcpSubagentsConfig, SubagentConfig, SubagentPresetConfig};
pub use error::{AcpClientError, HandshakeStep};

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::{
    Agent, Client, SessionMessage, on_receive_notification, on_receive_request,
    schema::{
        RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
        SelectedPermissionOutcome, SessionId, SessionNotification, StopReason,
    },
};
use futures::channel::mpsc;
use tokio::sync::oneshot;
use tracing::Instrument;

use driver::SubagentCommand;

/// The outcome of a completed sub-agent session.
///
/// Contains the concatenated text output and the final stop reason.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// All `AgentMessageChunk::Text` content concatenated in order.
    pub text: String,
    /// The reason the agent stopped generating.
    pub stop_reason: StopReason,
}

/// A live handle to a spawned ACP sub-agent session.
///
/// `SubagentHandle` serialises all ACP operations through a command channel that
/// is serviced by a background driver task. Concurrent reads are rejected with
/// [`AcpClientError::DriverBusy`]; callers must wait for the in-flight operation
/// to complete before issuing another read.
///
/// Dropping the handle without calling [`close`](Self::close) aborts the driver
/// task, which in turn kills the subprocess via `kill_on_drop`.
pub struct SubagentHandle {
    cmd_tx: mpsc::UnboundedSender<SubagentCommand>,
    join_handle: tokio::task::JoinHandle<()>,
    session_id: SessionId,
    closed: bool,
    prompt_timeout: Duration,
}

impl SubagentHandle {
    /// The ACP `SessionId` assigned by the sub-agent.
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Construct a handle wired to an external command channel.
    ///
    /// Used in tests to inject a mock driver without spawning a subprocess.
    #[cfg(test)]
    pub(crate) fn new_for_test(
        cmd_tx: mpsc::UnboundedSender<SubagentCommand>,
        join_handle: tokio::task::JoinHandle<()>,
        session_id: SessionId,
    ) -> Self {
        Self {
            cmd_tx,
            join_handle,
            session_id,
            closed: false,
            prompt_timeout: Duration::from_secs(30),
        }
    }

    /// Send a text prompt to the sub-agent.
    ///
    /// Returns immediately after enqueuing the prompt — the sub-agent will
    /// process it asynchronously. Call [`read_update`](Self::read_update) or
    /// [`read_to_string`](Self::read_to_string) to receive the response.
    ///
    /// # Errors
    ///
    /// Returns [`AcpClientError::Closed`] when the session has been closed,
    /// [`AcpClientError::DriverDied`] when the background driver has exited
    /// unexpectedly, or [`AcpClientError::Sdk`] for protocol errors.
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_acp::client::{SubagentConfig, spawn_subagent};
    ///
    /// # async fn example() -> Result<(), zeph_acp::client::AcpClientError> {
    /// let cfg = SubagentConfig { command: "zeph --acp".to_owned(), ..SubagentConfig::default() };
    /// let mut handle = spawn_subagent(cfg).await?;
    /// handle.send_prompt("What is 2 + 2?").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn send_prompt(&mut self, text: impl Into<String>) -> Result<(), AcpClientError> {
        if self.closed {
            return Err(AcpClientError::Closed);
        }
        let span = tracing::info_span!("acp.client.prompt");
        async {
            let (tx, rx) = oneshot::channel();
            self.cmd_tx
                .unbounded_send(SubagentCommand::Prompt {
                    text: text.into(),
                    reply: tx,
                })
                .map_err(|_| AcpClientError::DriverDied)?;
            rx.await.map_err(|_| AcpClientError::DriverDied)?
        }
        .instrument(span)
        .await
    }

    /// Read one `SessionMessage` update from the sub-agent.
    ///
    /// Blocks until an update arrives or the session closes. A concurrent call
    /// returns [`AcpClientError::DriverBusy`] immediately.
    ///
    /// # Errors
    ///
    /// Returns [`AcpClientError::Closed`] or [`AcpClientError::DriverDied`] when
    /// the session ends, or [`AcpClientError::DriverBusy`] when a concurrent read
    /// is already in progress.
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_acp::client::{SubagentConfig, spawn_subagent};
    ///
    /// # async fn example() -> Result<(), zeph_acp::client::AcpClientError> {
    /// let cfg = SubagentConfig { command: "zeph --acp".to_owned(), ..SubagentConfig::default() };
    /// let mut handle = spawn_subagent(cfg).await?;
    /// handle.send_prompt("hello").await?;
    /// let update = handle.read_update().await?;
    /// # drop(update);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn read_update(&mut self) -> Result<SessionMessage, AcpClientError> {
        if self.closed {
            return Err(AcpClientError::Closed);
        }
        let span = tracing::info_span!("acp.client.read_update");
        async {
            let (tx, rx) = oneshot::channel();
            self.cmd_tx
                .unbounded_send(SubagentCommand::ReadUpdate { reply: tx })
                .map_err(|_| AcpClientError::DriverDied)?;
            rx.await.map_err(|_| AcpClientError::DriverDied)?
        }
        .instrument(span)
        .await
    }

    /// Drain all updates until `StopReason`, collecting text into a [`RunOutcome`].
    ///
    /// Equivalent to calling `read_update` in a loop, filtering for text chunks and
    /// terminating on `StopReason`. Ignores thought chunks, tool calls, and plans.
    ///
    /// A [`send_cancel`](Self::send_cancel) issued concurrently will interrupt the
    /// drain and the in-flight read will resolve with `StopReason::Cancelled`.
    ///
    /// # Errors
    ///
    /// Returns [`AcpClientError::Closed`] or [`AcpClientError::DriverDied`] when
    /// the session ends, or [`AcpClientError::DriverBusy`] when another read is in
    /// progress.
    pub async fn read_to_string(&mut self) -> Result<RunOutcome, AcpClientError> {
        if self.closed {
            return Err(AcpClientError::Closed);
        }
        let timeout = self.prompt_timeout;
        let span = tracing::info_span!("acp.client.read_to_string");
        async {
            let (tx, rx) = oneshot::channel();
            self.cmd_tx
                .unbounded_send(SubagentCommand::ReadToString { reply: tx })
                .map_err(|_| AcpClientError::DriverDied)?;
            tokio::time::timeout(timeout, rx)
                .await
                .map_err(|_| AcpClientError::Timeout)?
                .map_err(|_| AcpClientError::DriverDied)?
        }
        .instrument(span)
        .await
    }

    /// Send a `session/cancel` notification to the sub-agent.
    ///
    /// This does not close the session; the sub-agent should acknowledge the cancel
    /// by sending a `StopReason::Cancelled` update on the active read.
    ///
    /// The one-poll-cycle preemption guarantee (a cancel delivered while
    /// [`read_update`](Self::read_update) or [`read_to_string`](Self::read_to_string) is blocked
    /// will interrupt the read within one `tokio::select!` cycle) only applies when one of those
    /// read operations is currently in progress. Calling `send_cancel` outside of an active read
    /// sends the ACP notification but does not interrupt any future read.
    ///
    /// # Errors
    ///
    /// Returns [`AcpClientError::Closed`] when the session is already closed or
    /// [`AcpClientError::DriverDied`] if the driver has exited.
    pub async fn send_cancel(&mut self) -> Result<(), AcpClientError> {
        if self.closed {
            return Err(AcpClientError::Closed);
        }
        let span = tracing::info_span!("acp.client.cancel");
        async {
            let (tx, rx) = oneshot::channel();
            self.cmd_tx
                .unbounded_send(SubagentCommand::Cancel { reply: tx })
                .map_err(|_| AcpClientError::DriverDied)?;
            rx.await.map_err(|_| AcpClientError::DriverDied)?
        }
        .instrument(span)
        .await
    }

    /// Close the session and wait for the driver to shut down.
    ///
    /// Idempotent: a second call returns [`AcpClientError::Closed`] immediately.
    ///
    /// # Errors
    ///
    /// Returns [`AcpClientError::DriverDied`] if the driver exited before the
    /// close acknowledgement was received.
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_acp::client::{SubagentConfig, spawn_subagent};
    ///
    /// # async fn example() -> Result<(), zeph_acp::client::AcpClientError> {
    /// let cfg = SubagentConfig { command: "zeph --acp".to_owned(), ..SubagentConfig::default() };
    /// let mut handle = spawn_subagent(cfg).await?;
    /// handle.close().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn close(&mut self) -> Result<(), AcpClientError> {
        if self.closed {
            return Err(AcpClientError::Closed);
        }
        self.closed = true;
        let span = tracing::info_span!("acp.client.close");
        async {
            let (tx, rx) = oneshot::channel();
            let _ = self
                .cmd_tx
                .unbounded_send(SubagentCommand::Close { ack: tx });
            // Best-effort acknowledgement with a short timeout — driver may have already exited.
            let _ = tokio::time::timeout(Duration::from_secs(5), rx).await;
            self.join_handle.abort();
            Ok(())
        }
        .instrument(span)
        .await
    }
}

impl Drop for SubagentHandle {
    fn drop(&mut self) {
        self.join_handle.abort();
    }
}

/// Spawn a sub-agent subprocess and complete the ACP handshake.
///
/// Returns a [`SubagentHandle`] once the `initialize` + `session/new` handshake
/// succeeds. The handle can then be used to send prompts and read responses.
///
/// The subprocess is spawned with a cleared environment (`env_clear`) so no
/// `ZEPH_*` secrets are forwarded. `kill_on_drop(true)` ensures the child is
/// reaped when the handle is dropped.
///
/// # Errors
///
/// Returns [`AcpClientError::InvalidConfig`] for bad command strings,
/// [`AcpClientError::Spawn`] for OS spawn failures,
/// [`AcpClientError::Handshake`] for protocol handshake failures,
/// or [`AcpClientError::Timeout`] when the handshake exceeds `handshake_timeout_secs`.
///
/// # Examples
///
/// ```no_run
/// use zeph_acp::client::{SubagentConfig, spawn_subagent};
///
/// # async fn example() -> Result<(), zeph_acp::client::AcpClientError> {
/// let cfg = SubagentConfig {
///     command: "zeph --acp".to_owned(),
///     auto_approve_permissions: true,
///     ..SubagentConfig::default()
/// };
/// let handle = spawn_subagent(cfg).await?;
/// # drop(handle);
/// # Ok(())
/// # }
/// ```
pub async fn spawn_subagent(cfg: SubagentConfig) -> Result<SubagentHandle, AcpClientError> {
    // The span covers only the handshake phase (up to session_id resolution).
    // The driver task lifetime is not included — it runs independently after this returns.
    let span = tracing::info_span!("acp.client.connect");
    spawn_subagent_inner(cfg).instrument(span).await
}

async fn spawn_subagent_inner(cfg: SubagentConfig) -> Result<SubagentHandle, AcpClientError> {
    let spawned = transport::spawn_child(&cfg)?;

    let (cmd_tx, cmd_rx) = mpsc::unbounded::<SubagentCommand>();
    let (ready_tx, ready_rx) = oneshot::channel::<Result<SessionId, AcpClientError>>();
    let ready_slot = Arc::new(Mutex::new(Some(ready_tx)));

    let transport = transport::make_byte_streams(spawned.stdin, spawned.stdout);
    let auto_approve = cfg.auto_approve_permissions;
    let handshake_timeout = Duration::from_secs(cfg.handshake_timeout_secs);
    let prompt_timeout = Duration::from_secs(cfg.prompt_timeout_secs);

    let ready_slot_clone = ready_slot.clone();
    let cfg_clone = cfg.clone();
    let child = spawned.child;
    let stderr_task = transport::spawn_stderr_drain(spawned.stderr, "pending".to_owned());

    let join_handle =
        tokio::spawn(async move {
            let result = Client
            .builder()
            .on_receive_notification(
                async move |_notif: SessionNotification, _cx| Ok(()),
                on_receive_notification!(),
            )
            .on_receive_request(
                async move |req: RequestPermissionRequest,
                      responder: agent_client_protocol::Responder<RequestPermissionResponse>,
                      _cx: agent_client_protocol::ConnectionTo<Agent>| {
                    let outcome = if auto_approve {
                        if let Some(opt) = req.options.first() {
                            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                                opt.option_id.clone(),
                            ))
                        } else {
                            RequestPermissionOutcome::Cancelled
                        }
                    } else {
                        RequestPermissionOutcome::Cancelled
                    };
                    let _ = responder.respond(RequestPermissionResponse::new(outcome));
                    Ok(())
                },
                on_receive_request!(),
            )
            .connect_with(transport, move |cx: agent_client_protocol::ConnectionTo<Agent>| {
                let ready_slot = ready_slot_clone;
                let cfg = cfg_clone;
                async move {
                    driver::run_driver(cx, cmd_rx, ready_slot, cfg, child, stderr_task).await
                }
            })
            .await;

            if let Err(e) = result {
                tracing::debug!(error = %e, "acp.client.connect: transport closed");
            }
        });

    // Wait for the handshake to complete (or fail) within the timeout.
    // On timeout we abort the spawned task so the child process and stderr-drain
    // task are cleaned up rather than leaking as zombies.
    let session_id = match tokio::time::timeout(handshake_timeout, ready_rx).await {
        Ok(Ok(Ok(id))) => id,
        Ok(Ok(Err(e))) => {
            join_handle.abort();
            return Err(e);
        }
        Ok(Err(_)) => {
            join_handle.abort();
            return Err(AcpClientError::DriverDied);
        }
        Err(_) => {
            join_handle.abort();
            return Err(AcpClientError::Timeout);
        }
    };

    Ok(SubagentHandle {
        cmd_tx,
        join_handle,
        session_id,
        closed: false,
        prompt_timeout,
    })
}

/// Convenience function: spawn a sub-agent, send one prompt, and drain to string.
///
/// Wraps [`spawn_subagent`] + [`SubagentHandle::send_prompt`] +
/// [`SubagentHandle::read_to_string`] into a single call. The session is
/// closed after the response is received.
///
/// # Errors
///
/// Propagates any error from the underlying handle methods.
///
/// # Examples
///
/// ```no_run
/// use zeph_acp::client::{SubagentConfig, run_session};
///
/// # async fn example() -> Result<(), zeph_acp::client::AcpClientError> {
/// let cfg = SubagentConfig {
///     command: "zeph --acp".to_owned(),
///     auto_approve_permissions: true,
///     ..SubagentConfig::default()
/// };
/// let outcome = run_session(cfg, "What is 2 + 2?").await?;
/// println!("{}", outcome.text);
/// # Ok(())
/// # }
/// ```
pub async fn run_session(
    cfg: SubagentConfig,
    prompt: impl Into<String>,
) -> Result<RunOutcome, AcpClientError> {
    let span = tracing::info_span!("acp.client.session.run");
    run_session_inner(cfg, prompt.into()).instrument(span).await
}

async fn run_session_inner(
    cfg: SubagentConfig,
    prompt: String,
) -> Result<RunOutcome, AcpClientError> {
    let session_timeout = Duration::from_secs(cfg.session_timeout_secs);
    let mut handle = spawn_subagent(cfg).await?;
    let result = tokio::time::timeout(session_timeout, async {
        handle.send_prompt(prompt).await?;
        handle.read_to_string().await
    })
    .await
    .map_err(|_| AcpClientError::Timeout)?;
    let _ = handle.close().await;
    result
}

#[cfg(test)]
mod tests {
    include!("tests.rs");
}
