// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Driver loop and command channel for the ACP sub-agent client.
//!
//! The driver runs inside `connect_with`'s closure and services a
//! `SubagentCommand` channel that `SubagentHandle` writes to. A biased
//! `tokio::select!` inside read operations guarantees that a `Cancel` command
//! preempts an in-flight `read_update` within one poll cycle.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::{
    ActiveSession, Agent, ConnectionTo, SessionMessage,
    schema::{
        CancelNotification, ContentBlock, ContentChunk, InitializeRequest, ProtocolVersion,
        SessionId, SessionNotification, SessionUpdate,
    },
};
use futures::StreamExt;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::client::{AcpClientError, HandshakeStep, RunOutcome, SubagentConfig};

type ReadySlot = Arc<Mutex<Option<oneshot::Sender<Result<SessionId, AcpClientError>>>>>;

/// Commands sent from [`super::SubagentHandle`] to the driver.
pub(crate) enum SubagentCommand {
    /// Send a text prompt to the sub-agent.
    Prompt {
        text: String,
        reply: oneshot::Sender<Result<(), AcpClientError>>,
    },
    /// Read one `SessionMessage` update from the sub-agent.
    ReadUpdate {
        reply: oneshot::Sender<Result<SessionMessage, AcpClientError>>,
    },
    /// Drain all updates until `StopReason`, collecting text chunks.
    ReadToString {
        reply: oneshot::Sender<Result<RunOutcome, AcpClientError>>,
    },
    /// Send a `session/cancel` notification to the sub-agent.
    Cancel {
        reply: oneshot::Sender<Result<(), AcpClientError>>,
    },
    /// Close the session and shut down the driver.
    Close { ack: oneshot::Sender<()> },
}

/// Run the full ACP handshake + command loop for a single sub-agent session.
///
/// This function is intended to be called inside `Client.builder().connect_with(transport, ...)`.
/// It:
/// 1. Sends `InitializeRequest` and waits for the response.
/// 2. Builds a session via `build_session(session_cwd).block_task().run_until(...)`.
/// 3. On success, fires `ready_tx` with `Ok(session_id)`.
/// 4. Enters the command loop, servicing `SubagentCommand` messages until `Close`.
/// 5. Kills and reaps the child process, aborts the stderr drain task.
///
/// Any handshake failure fires `ready_tx` with an `Err` variant so the caller
/// receives a typed `AcpClientError::Handshake` rather than a silent `DriverDied`.
pub(crate) async fn run_driver(
    cx: ConnectionTo<Agent>,
    cmd_rx: futures::channel::mpsc::UnboundedReceiver<SubagentCommand>,
    ready_slot: ReadySlot,
    cfg: SubagentConfig,
    mut child: tokio::process::Child,
    stderr_task: JoinHandle<()>,
) -> Result<(), agent_client_protocol::Error> {
    let cx_clone = cx.clone();

    // Stage 1: Initialize.
    let init_result = tokio::time::timeout(
        Duration::from_secs(cfg.handshake_timeout_secs),
        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task(),
    )
    .await;

    match init_result {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            fire_handshake_err(&ready_slot, HandshakeStep::Initialize, e);
            cleanup_child(&mut child, stderr_task).await;
            return Ok(());
        }
        Err(_) => {
            fire_handshake_err(
                &ready_slot,
                HandshakeStep::Initialize,
                agent_client_protocol::Error::internal_error().data("initialize timed out"),
            );
            cleanup_child(&mut child, stderr_task).await;
            return Ok(());
        }
    }

    let session_cwd = cfg.effective_session_cwd();

    // Stage 2 + 3: build_session / run_until.
    // `ready_slot` is taken inside the closure on success; if `run_until` returns
    // `Err` without the body ever running, the outer code fires the handshake error.
    let ready_for_body = ready_slot.clone();
    let cmd_rx = Arc::new(Mutex::new(Some(cmd_rx)));
    let cmd_rx_for_body = cmd_rx.clone();

    let run_result = cx_clone
        .build_session(session_cwd)
        .block_task()
        .run_until(async move |mut session: ActiveSession<'_, Agent>| {
            // Session is up — notify the waiting caller.
            if let Some(tx) = ready_for_body
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                let _ = tx.send(Ok(session.session_id().clone()));
            }

            // Extract cmd_rx from the Arc<Mutex<Option<_>>>.
            let Some(mut cmd_rx) = cmd_rx_for_body
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            else {
                return Ok(());
            };

            // Command loop.
            while let Some(cmd) = cmd_rx.next().await {
                match cmd {
                    SubagentCommand::Prompt { text, reply } => {
                        let r = session.send_prompt(text).map_err(AcpClientError::Sdk);
                        let _ = reply.send(r);
                    }
                    SubagentCommand::ReadUpdate { reply } => {
                        let r = read_one_with_preemption(&mut session, &mut cmd_rx).await;
                        let _ = reply.send(r);
                    }
                    SubagentCommand::ReadToString { reply } => {
                        let r = drain_until_stop(&mut session, &mut cmd_rx).await;
                        let _ = reply.send(r);
                    }
                    SubagentCommand::Cancel { reply } => {
                        let r = session
                            .connection()
                            .send_notification(CancelNotification::new(
                                session.session_id().clone(),
                            ))
                            .map_err(AcpClientError::Sdk);
                        let _ = reply.send(r);
                    }
                    SubagentCommand::Close { ack } => {
                        let _ = ack.send(());
                        break;
                    }
                }
            }
            Ok(())
        })
        .await;

    // If run_until failed before the body could run, surface a handshake error.
    if let (Err(e), Some(tx)) = (
        run_result.as_ref(),
        ready_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take(),
    ) {
        let _ = tx.send(Err(AcpClientError::Handshake {
            step: HandshakeStep::NewSession,
            source: e.clone(),
        }));
    }

    cleanup_child(&mut child, stderr_task).await;

    run_result
}

/// Read a single `SessionMessage`, allowing Cancel/Close to preempt the read.
///
/// Uses a `biased` `tokio::select!` so that an incoming command is always checked
/// before the read future, bounding cancellation latency to one poll cycle.
///
/// When a non-Cancel/Close command arrives while reading, this function replies
/// `DriverBusy` to *that* command's caller and continues waiting for the update.
pub(crate) async fn read_one_with_preemption(
    session: &mut ActiveSession<'_, Agent>,
    cmd_rx: &mut futures::channel::mpsc::UnboundedReceiver<SubagentCommand>,
) -> Result<SessionMessage, AcpClientError> {
    loop {
        tokio::select! {
            biased;

            maybe = cmd_rx.next() => match maybe {
                Some(SubagentCommand::Cancel { reply }) => {
                    // Send cancel notification inline, then wait for the real StopReason update.
                    let r = session
                        .connection()
                        .send_notification(CancelNotification::new(session.session_id().clone()))
                        .map_err(AcpClientError::Sdk);
                    let _ = reply.send(r);
                    // Fall through to read the cancel acknowledgement from the agent.
                    return session.read_update().await.map_err(AcpClientError::Sdk);
                }
                Some(SubagentCommand::Close { ack }) => {
                    let _ = ack.send(());
                    return Err(AcpClientError::Closed);
                }
                Some(other) => {
                    // Reply DriverBusy to the incoming command, keep waiting for update.
                    send_busy(other);
                }
                None => return Err(AcpClientError::Closed),
            },

            update = session.read_update() => {
                return update.map_err(AcpClientError::Sdk);
            }
        }
    }
}

/// Drain all `SessionMessage` updates until `StopReason`, collecting text chunks.
///
/// Mirrors the pattern of `ActiveSession::read_to_string` (SDK `session.rs:591`)
/// but exposes `StopReason` via [`RunOutcome`]. Uses the same biased select loop
/// as [`read_one_with_preemption`] so Cancel commands preempt the read.
///
/// Included in `text`:
/// - `SessionUpdate::AgentMessageChunk` with `ContentBlock::Text`.
///
/// Ignored (span event emitted but not concatenated):
/// - Non-text `AgentMessageChunk` variants.
/// - `AgentThoughtChunk` variants.
/// - `ToolCall`, `ToolCallUpdate`, `Plan`, and any `#[non_exhaustive]` unknown variants.
pub(crate) async fn drain_until_stop(
    session: &mut ActiveSession<'_, Agent>,
    cmd_rx: &mut futures::channel::mpsc::UnboundedReceiver<SubagentCommand>,
) -> Result<RunOutcome, AcpClientError> {
    use agent_client_protocol::util::MatchDispatch;

    let mut text = String::new();

    loop {
        let update = {
            loop {
                tokio::select! {
                    biased;

                    maybe = cmd_rx.next() => match maybe {
                        Some(SubagentCommand::Cancel { reply }) => {
                            let r = session
                                .connection()
                                .send_notification(CancelNotification::new(session.session_id().clone()))
                                .map_err(AcpClientError::Sdk);
                            let _ = reply.send(r);
                            let upd = session.read_update().await.map_err(AcpClientError::Sdk)?;
                            break upd;
                        }
                        Some(SubagentCommand::Close { ack }) => {
                            let _ = ack.send(());
                            return Err(AcpClientError::Closed);
                        }
                        Some(other) => {
                            send_busy(other);
                        }
                        None => return Err(AcpClientError::Closed),
                    },

                    upd = session.read_update() => {
                        break upd.map_err(AcpClientError::Sdk)?;
                    }
                }
            }
        };

        match update {
            SessionMessage::SessionMessage(dispatch) => {
                MatchDispatch::new(dispatch)
                    .if_notification(async |notif: SessionNotification| {
                        match notif.update {
                            SessionUpdate::AgentMessageChunk(ContentChunk {
                                content: ContentBlock::Text(t),
                                ..
                            }) => {
                                text.push_str(&t.text);
                            }
                            SessionUpdate::AgentThoughtChunk(_) => {
                                tracing::trace!(target: "acp.client.drain", "thought_chunk ignored");
                            }
                            SessionUpdate::ToolCall(ref tc) => {
                                tracing::trace!(
                                    target: "acp.client.drain",
                                    tool_call_id = ?tc.tool_call_id,
                                    "tool_call ignored"
                                );
                            }
                            SessionUpdate::Plan(_) => {
                                tracing::trace!(target: "acp.client.drain", "plan ignored");
                            }
                            _ => {
                                tracing::debug!(target: "acp.client.drain", "unknown SessionUpdate variant ignored");
                            }
                        }
                        Ok(())
                    })
                    .await
                    .otherwise_ignore()
                    .map_err(AcpClientError::Sdk)?;
            }
            SessionMessage::StopReason(reason) => {
                return Ok(RunOutcome {
                    text,
                    stop_reason: reason,
                });
            }
            _ => {
                tracing::debug!(target: "acp.client.drain", "unknown SessionMessage variant ignored");
            }
        }
    }
}

fn fire_handshake_err(slot: &ReadySlot, step: HandshakeStep, source: agent_client_protocol::Error) {
    if let Some(tx) = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        let _ = tx.send(Err(AcpClientError::Handshake { step, source }));
    }
}

async fn cleanup_child(child: &mut tokio::process::Child, stderr_task: JoinHandle<()>) {
    let _ = child.start_kill();
    let _ = child.wait().await;
    stderr_task.abort();
}

/// Reply `DriverBusy` to an incoming command that arrived while a read is in progress.
fn send_busy(cmd: SubagentCommand) {
    match cmd {
        SubagentCommand::Prompt { reply, .. } => {
            let _ = reply.send(Err(AcpClientError::DriverBusy));
        }
        SubagentCommand::ReadUpdate { reply } => {
            let _ = reply.send(Err(AcpClientError::DriverBusy));
        }
        SubagentCommand::ReadToString { reply } => {
            let _ = reply.send(Err(AcpClientError::DriverBusy));
        }
        SubagentCommand::Cancel { .. } => {
            // Cancel is consumed by the biased select arm above; this arm is unreachable.
            unreachable!("Cancel must be handled by the biased select arm, not send_busy");
        }
        SubagentCommand::Close { ack } => {
            // Honour close even during a read; the loop will notice and return.
            let _ = ack.send(());
        }
    }
}
