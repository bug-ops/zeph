// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! IDE elicitation bridge for ACP sessions.
//!
//! When the IDE advertises elicitation capability during `initialize()`, each new session
//! spawns a bridge task that accepts [`ElicitationRequest`]s from the agent loop, forwards
//! them to the IDE via `elicitation/create`, and returns the IDE's response via a oneshot
//! channel.
//!
//! The bridge task listens on a `cancel_signal` to terminate when the session is closed,
//! preventing task leaks on session reap or cancel.

use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol as acp;
use agent_client_protocol_schema as schema;
use tokio::sync::{mpsc, oneshot};

use crate::error::AcpError;

/// A pending elicitation request from the agent loop to the IDE.
pub(crate) struct ElicitationRequest {
    /// The elicitation form request to forward to the IDE.
    pub create_req: schema::CreateElicitationRequest,
    /// Channel to send the IDE's response back to the agent loop.
    pub response_tx: oneshot::Sender<Result<schema::CreateElicitationResponse, AcpError>>,
}

/// Sender half for delivering elicitation requests to the bridge task.
pub(crate) type ElicitationSender = mpsc::Sender<ElicitationRequest>;
/// Receiver half consumed by the bridge task.
pub(crate) type ElicitationReceiver = mpsc::Receiver<ElicitationRequest>;

/// Create a bounded elicitation channel pair.
///
/// Capacity 4 is sufficient since elicitations are sequential (the agent loop awaits
/// each response before sending the next request).
pub(crate) fn elicitation_channel() -> (ElicitationSender, ElicitationReceiver) {
    mpsc::channel(4)
}

/// Spawn the per-session elicitation bridge task.
///
/// The task forwards each [`ElicitationRequest`] to the IDE and returns the response
/// via the embedded oneshot channel. It terminates cleanly when:
/// - `elicitation_rx` is dropped (session entry dropped, channel closed).
/// - `cancel_signal` is notified (session cancelled or reaped).
///
/// # Arguments
///
/// - `cx`: ACP connection to the IDE.
/// - `elicitation_rx`: Receives elicitation requests from the agent loop.
/// - `cancel_signal`: Session cancel notify — bridge exits on notification.
/// - `timeout_secs`: Per-request timeout from `[acp.timeouts].elicitation_secs`.
pub(crate) fn spawn_elicitation_bridge(
    cx: acp::ConnectionTo<acp::Client>,
    mut elicitation_rx: ElicitationReceiver,
    cancel_signal: Arc<tokio::sync::Notify>,
    timeout_secs: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                () = cancel_signal.notified() => {
                    tracing::debug!("elicitation bridge: session cancelled, exiting");
                    break;
                }
                req = elicitation_rx.recv() => {
                    let Some(ElicitationRequest { create_req, response_tx }) = req else {
                        // Sender dropped — session entry is gone.
                        tracing::debug!("elicitation bridge: channel closed, exiting");
                        break;
                    };
                    let result = send_elicitation(&cx, create_req, timeout_secs).await;
                    // Ignore send failure — agent loop may have timed out on its side.
                    let _ = response_tx.send(result);
                }
            }
        }
    })
}

/// Send an elicitation request to the IDE and await the response within `timeout_secs`.
///
/// `CreateElicitationRequest` is not yet registered in the SDK's typed dispatch table, so the
/// request is sent as an `UntypedMessage` and the response is deserialized from `serde_json::Value`.
async fn send_elicitation(
    cx: &acp::ConnectionTo<acp::Client>,
    req: schema::CreateElicitationRequest,
    timeout_secs: u64,
) -> Result<schema::CreateElicitationResponse, AcpError> {
    let params = serde_json::to_value(&req).map_err(|e| AcpError::ClientError(e.to_string()))?;
    let msg = acp::UntypedMessage::new("elicitation/create", params)
        .map_err(|e| AcpError::ClientError(e.to_string()))?;
    let timeout = Duration::from_secs(timeout_secs);
    let raw: serde_json::Value = tokio::time::timeout(timeout, cx.send_request(msg).block_task())
        .await
        .map_err(|_| AcpError::ChannelClosed)?
        .map_err(|e| AcpError::ClientError(e.to_string()))?;
    serde_json::from_value(raw).map_err(|e| AcpError::ClientError(e.to_string()))
}

/// Extension methods for the elicitation bridge in [`crate::AcpContext`].
///
/// Available only when the `unstable-elicitation` feature is enabled and the IDE
/// advertised elicitation capability during `initialize()`.
#[allow(dead_code)]
pub(crate) struct ElicitationBridge {
    /// Sender to the bridge task for this session.
    pub tx: ElicitationSender,
    /// Timeout applied to each elicitation round-trip.
    pub timeout_secs: u64,
}

impl ElicitationBridge {
    /// Send a form elicitation to the IDE and await the user's response.
    ///
    /// Returns the [`schema::ElicitationAction`] chosen by the user (accept, decline,
    /// or cancel). The per-request timeout is enforced inside the bridge task via
    /// `spawn_elicitation_bridge`; this method returns as soon as the bridge responds.
    ///
    /// # Errors
    ///
    /// Returns [`AcpError::ChannelClosed`] when the bridge task has exited (session
    /// cancelled or sender dropped), or [`AcpError::ClientError`] when the IDE returns
    /// an error response or the bridge-side timeout fires.
    #[allow(dead_code)]
    pub(crate) async fn elicit(
        &self,
        session_id: Arc<schema::SessionId>,
        message: impl Into<String>,
        requested_schema: schema::ElicitationSchema,
    ) -> Result<schema::ElicitationAction, AcpError> {
        let scope = schema::ElicitationScope::Session(schema::ElicitationSessionScope::new(
            session_id.to_string(),
        ));
        let mode = schema::ElicitationFormMode::new(scope, requested_schema);
        let create_req = schema::CreateElicitationRequest::new(mode, message);
        let (response_tx, response_rx) = oneshot::channel();
        self.tx
            .send(ElicitationRequest {
                create_req,
                response_tx,
            })
            .await
            .map_err(|_| AcpError::ChannelClosed)?;

        let resp = response_rx.await.map_err(|_| AcpError::ChannelClosed)??;

        Ok(resp.action)
    }
}
