// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

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
use crate::transport::bridge::spawn_acp_connection;
#[cfg(feature = "acp-http")]
use crate::transport::http::AcpHttpState;

#[cfg(feature = "acp-http")]
const WS_MAX_MESSAGE_SIZE: usize = 1_048_576; // 1 MiB

/// `GET /acp/ws` — WebSocket upgrade handler.
///
/// Rejects with `503 Service Unavailable` when `max_sessions` is reached.
/// Each accepted connection spawns a dedicated ACP bridge thread.
#[cfg(feature = "acp-http")]
pub async fn ws_upgrade_handler(
    ws: WebSocketUpgrade,
    State(state): State<AcpHttpState>,
) -> Response {
    if state.connections.len() >= state.server_config.max_sessions {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    ws.max_message_size(WS_MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| handle_ws(socket, state))
        .into_response()
}

#[cfg(feature = "acp-http")]
async fn handle_ws(socket: WebSocket, state: AcpHttpState) {
    let (reader, mut writer) =
        spawn_acp_connection(state.spawner.clone(), (*state.server_config).clone());

    let (mut ws_tx, mut ws_rx) = socket.split();

    let ws_to_agent = async move {
        while let Some(Ok(msg)) = ws_rx.next().await {
            if let Message::Text(text) = msg {
                if writer.write_all(text.as_bytes()).await.is_err() {
                    break;
                }
                if writer.write_all(b"\n").await.is_err() {
                    break;
                }
            }
        }
    };

    let agent_to_ws = async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if ws_tx.send(Message::Text(line.into())).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        () = ws_to_agent => {},
        () = agent_to_ws => {},
    }
}
