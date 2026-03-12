// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(feature = "acp-http")]
use std::sync::Arc;
#[cfg(feature = "acp-http")]
use std::sync::atomic::Ordering;
#[cfg(feature = "acp-http")]
use std::time::Duration;

#[cfg(feature = "acp-http")]
use axum::extract::State;
#[cfg(feature = "acp-http")]
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
#[cfg(feature = "acp-http")]
use axum::http::StatusCode;
#[cfg(feature = "acp-http")]
use axum::response::{IntoResponse, Response};
#[cfg(feature = "acp-http")]
use futures::SinkExt as _;
#[cfg(feature = "acp-http")]
use futures::StreamExt as _;
#[cfg(feature = "acp-http")]
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(feature = "acp-http")]
use tokio::sync::mpsc;

#[cfg(feature = "acp-http")]
use std::sync::atomic::AtomicU64;

#[cfg(feature = "acp-http")]
use crate::transport::bridge::spawn_acp_connection;
#[cfg(feature = "acp-http")]
use crate::transport::http::{AcpHttpState, ConnectionHandle};

#[cfg(feature = "acp-http")]
const WS_MAX_MESSAGE_SIZE: usize = 1_048_576; // 1 MiB

/// Ping interval and pong timeout for WebSocket keepalive.
#[cfg(feature = "acp-http")]
const WS_PING_INTERVAL: Duration = Duration::from_secs(30);
#[cfg(feature = "acp-http")]
const WS_PONG_TIMEOUT: Duration = Duration::from_secs(90);

/// `GET /acp/ws` — WebSocket upgrade handler.
///
/// Rejects with `503 Service Unavailable` when `max_sessions` is reached.
/// Each accepted connection spawns a dedicated ACP bridge thread and is registered
/// in the shared session map for lifecycle tracking.
///
/// Uses an `AtomicUsize` slot reservation before the upgrade handshake to avoid
/// TOCTOU between the capacity check and the `DashMap` insertion.
#[cfg(feature = "acp-http")]
pub async fn ws_upgrade_handler(
    ws: WebSocketUpgrade,
    State(state): State<AcpHttpState>,
) -> Response {
    if !state.ready.load(Ordering::Acquire) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if !state.try_reserve_ws_slot() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    ws.max_message_size(WS_MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| handle_ws(socket, state))
        .into_response()
}

/// Outbound frame type for the WS write task.
#[cfg(feature = "acp-http")]
enum WsFrame {
    Text(String),
    Ping,
    Pong(axum::body::Bytes),
    Close(Option<axum::extract::ws::CloseFrame>),
}

#[cfg(feature = "acp-http")]
fn register_ws_session(state: &AcpHttpState, session_id: &str) {
    let (placeholder_tx, _) = tokio::sync::broadcast::channel(1);
    let (_, placeholder_w) = tokio::io::duplex(1);
    state.connections.insert(
        session_id.to_owned(),
        Arc::new(ConnectionHandle {
            writer: Arc::new(tokio::sync::Mutex::new(placeholder_w)),
            output_tx: placeholder_tx,
            last_activity: AtomicU64::new(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            ),
            idle_timeout_secs: state.server_config.session_idle_timeout_secs,
        }),
    );
}

#[cfg(feature = "acp-http")]
async fn handle_ws(socket: WebSocket, state: AcpHttpState) {
    let session_id = uuid::Uuid::new_v4().to_string();
    register_ws_session(&state, &session_id);

    let (reader, mut writer) =
        spawn_acp_connection(state.spawner.clone(), (*state.server_config).clone());

    let (mut ws_tx, mut ws_rx) = socket.split();

    // Channel for sending outbound WS frames from read task to write task.
    let (frame_tx, mut frame_rx) = mpsc::channel::<WsFrame>(64);

    let frame_tx_ping = frame_tx.clone();
    let frame_tx_rx = frame_tx.clone();
    let session_id_read = session_id.clone();
    let state_cleanup = state.clone();

    // Read task: receives WS frames, forwards text to agent, handles ping/pong/close.
    let read_task = tokio::spawn(async move {
        let mut ping_tick = tokio::time::interval(WS_PING_INTERVAL);
        let mut last_pong_at = tokio::time::Instant::now();

        loop {
            tokio::select! {
                _ = ping_tick.tick() => {
                    if last_pong_at.elapsed() > WS_PONG_TIMEOUT {
                        tracing::warn!(
                            session = %session_id_read,
                            "WS pong timeout, closing connection"
                        );
                        break;
                    }
                    if frame_tx_ping.send(WsFrame::Ping).await.is_err() {
                        break;
                    }
                }
                msg = ws_rx.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if writer.write_all(text.as_bytes()).await.is_err() {
                                break;
                            }
                            if writer.write_all(b"\n").await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(Message::Pong(_))) => {
                            last_pong_at = tokio::time::Instant::now();
                        }
                        Some(Ok(Message::Ping(data))) => {
                            if frame_tx_rx.send(WsFrame::Pong(data)).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            break;
                        }
                        Some(Ok(Message::Binary(_))) => {
                            let _ = frame_tx_rx
                                .send(WsFrame::Close(Some(axum::extract::ws::CloseFrame {
                                    code: 1003,
                                    reason: "binary frames not supported".into(),
                                })))
                                .await;
                            break;
                        }
                        Some(Err(_)) => break,
                    }
                }
            }
        }

        state_cleanup.remove_connection(&session_id_read);
    });

    // Agent-to-WS task: reads agent output lines and enqueues text frames.
    let frame_tx_agent = frame_tx;
    let agent_task = tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if frame_tx_agent.send(WsFrame::Text(line)).await.is_err() {
                break;
            }
        }
    });

    // Write task: drains the frame channel and sends to the WS sink.
    let write_task = tokio::spawn(async move {
        while let Some(frame) = frame_rx.recv().await {
            let msg = match frame {
                WsFrame::Text(t) => Message::Text(t.into()),
                WsFrame::Ping => Message::Ping(vec![].into()),
                WsFrame::Pong(d) => Message::Pong(d),
                WsFrame::Close(f) => Message::Close(f),
            };
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    tokio::select! {
        _ = read_task => {},
        _ = agent_task => {},
    }

    // Give the write task up to 1 second to flush any buffered frames (e.g. close frame)
    // before dropping the sink.
    let _ = tokio::time::timeout(Duration::from_secs(1), write_task).await;

    // Ensure cleanup on any exit path.
    state.remove_connection(&session_id);
    state.release_ws_slot();
}
