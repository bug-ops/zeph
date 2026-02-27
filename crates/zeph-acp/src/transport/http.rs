// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(feature = "acp-http")]
use std::sync::Arc;
#[cfg(feature = "acp-http")]
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
#[cfg(feature = "acp-http")]
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(feature = "acp-http")]
use axum::extract::State;
#[cfg(feature = "acp-http")]
use axum::http::{HeaderMap, StatusCode};
#[cfg(feature = "acp-http")]
use axum::response::IntoResponse;
#[cfg(feature = "acp-http")]
use axum::response::sse::{Event, KeepAlive, Sse};
#[cfg(feature = "acp-http")]
use dashmap::DashMap;
#[cfg(feature = "acp-http")]
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
#[cfg(feature = "acp-http")]
use tokio::sync::{Mutex, broadcast};

#[cfg(feature = "acp-http")]
use axum::Json;
#[cfg(feature = "acp-http")]
use axum::extract::Path;
#[cfg(feature = "acp-http")]
use serde::Serialize;
#[cfg(feature = "acp-http")]
use zeph_memory::sqlite::{AcpSessionInfo, SqliteStore};

#[cfg(feature = "acp-http")]
use crate::agent::SendAgentSpawner;
#[cfg(feature = "acp-http")]
use crate::transport::{AcpServerConfig, bridge::spawn_acp_connection};

#[cfg(feature = "acp-http")]
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Handle for an active HTTP+SSE connection.
#[cfg(feature = "acp-http")]
pub(crate) struct ConnectionHandle {
    pub(crate) writer: Arc<Mutex<DuplexStream>>,
    pub(crate) output_tx: broadcast::Sender<String>,
    /// Unix timestamp (seconds) of last successful write from a client request.
    pub(crate) last_activity: AtomicU64,
    pub(crate) idle_timeout_secs: u64,
}

#[cfg(feature = "acp-http")]
impl ConnectionHandle {
    fn is_expired(&self) -> bool {
        let last = self.last_activity.load(Ordering::Relaxed);
        now_secs().saturating_sub(last) > self.idle_timeout_secs
    }

    fn touch(&self) {
        self.last_activity.store(now_secs(), Ordering::Relaxed);
    }
}

/// Serializable session metadata for the REST session list endpoint.
#[cfg(feature = "acp-http")]
#[derive(Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: i64,
}

#[cfg(feature = "acp-http")]
impl From<AcpSessionInfo> for SessionSummary {
    fn from(info: AcpSessionInfo) -> Self {
        Self {
            id: info.id,
            title: info.title,
            created_at: info.created_at,
            updated_at: info.updated_at,
            message_count: info.message_count,
        }
    }
}

/// Serializable event for the REST session messages endpoint.
#[cfg(feature = "acp-http")]
#[derive(Serialize)]
pub struct SessionEventDto {
    pub event_type: String,
    pub payload: String,
    pub created_at: String,
}

/// Shared state for the HTTP+SSE transport, held in axum `State`.
#[cfg(feature = "acp-http")]
#[derive(Clone)]
pub struct AcpHttpState {
    pub(crate) connections: Arc<DashMap<String, Arc<ConnectionHandle>>>,
    pub spawner: SendAgentSpawner,
    pub server_config: Arc<AcpServerConfig>,
    /// Atomic counter for active WebSocket sessions.
    /// Used to atomically reserve a slot before the upgrade handshake, eliminating TOCTOU
    /// between the capacity check and the actual `DashMap` insertion.
    pub(crate) active_ws: Arc<AtomicUsize>,
    /// Optional `SQLite` store for session history REST endpoints.
    pub store: Option<Arc<SqliteStore>>,
}

#[cfg(feature = "acp-http")]
impl AcpHttpState {
    pub fn new(spawner: SendAgentSpawner, server_config: AcpServerConfig) -> Self {
        Self {
            connections: Arc::new(DashMap::new()),
            spawner,
            server_config: Arc::new(server_config),
            active_ws: Arc::new(AtomicUsize::new(0)),
            store: None,
        }
    }

    #[must_use]
    pub fn with_store(mut self, store: SqliteStore) -> Self {
        self.store = Some(Arc::new(store));
        self
    }

    /// Try to atomically reserve a WebSocket session slot.
    ///
    /// Returns `true` and increments the counter if a slot is available.
    /// Returns `false` if `max_sessions` is already reached, without modifying the counter.
    pub(crate) fn try_reserve_ws_slot(&self) -> bool {
        let max = self.server_config.max_sessions;
        // Saturating loop: attempt CAS until either we claim a slot or find it full.
        let mut current = self.active_ws.load(Ordering::Relaxed);
        loop {
            if current >= max {
                return false;
            }
            match self.active_ws.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    /// Release a previously reserved WebSocket session slot.
    pub(crate) fn release_ws_slot(&self) {
        self.active_ws.fetch_sub(1, Ordering::AcqRel);
    }

    /// Remove a connection from the session map immediately (e.g. on WebSocket disconnect).
    pub(crate) fn remove_connection(&self, id: &str) {
        self.connections.remove(id);
    }

    /// Spawn a background task that reaps idle connections every 60 seconds.
    pub fn start_reaper(&self) {
        let connections = Arc::clone(&self.connections);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                connections.retain(|_, handle| !handle.is_expired());
            }
        });
    }
}

/// Create a new HTTP+SSE connection.
///
/// # Errors
///
/// Returns `503 Service Unavailable` when `max_sessions` is already reached.
#[cfg(feature = "acp-http")]
pub(crate) fn create_connection(
    state: &AcpHttpState,
) -> Result<(String, Arc<ConnectionHandle>), StatusCode> {
    if state.connections.len() >= state.server_config.max_sessions {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    let (reader, writer) =
        spawn_acp_connection(state.spawner.clone(), (*state.server_config).clone());

    let (tx, _) = broadcast::channel(256);
    let tx2 = tx.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = tx2.send(line);
        }
    });

    let session_id = uuid::Uuid::new_v4().to_string();
    let handle = Arc::new(ConnectionHandle {
        writer: Arc::new(Mutex::new(writer)),
        output_tx: tx,
        last_activity: AtomicU64::new(now_secs()),
        idle_timeout_secs: state.server_config.session_idle_timeout_secs,
    });

    state
        .connections
        .insert(session_id.clone(), Arc::clone(&handle));
    Ok((session_id, handle))
}

/// `POST /acp` — receive a JSON-RPC request line, stream responses as SSE.
///
/// If `Acp-Session-Id` header is present, routes to the existing connection.
/// Otherwise creates a new connection and returns `Acp-Session-Id` in response headers.
///
/// # Errors
///
/// Returns `400 Bad Request` if `Acp-Session-Id` is present but not a valid UUID.
/// Returns `404 Not Found` if `Acp-Session-Id` is given but not found.
/// Returns `500 Internal Server Error` if writing to the agent channel fails.
/// Returns `503 Service Unavailable` if `max_sessions` is reached.
#[cfg(feature = "acp-http")]
pub async fn post_handler(
    State(state): State<AcpHttpState>,
    headers: HeaderMap,
    body: String,
) -> Result<impl IntoResponse, StatusCode> {
    let (session_id, handle) =
        if let Some(id) = headers.get("acp-session-id").and_then(|v| v.to_str().ok()) {
            uuid::Uuid::parse_str(id).map_err(|_| StatusCode::BAD_REQUEST)?;
            let handle = state
                .connections
                .get(id)
                .map(|r| Arc::clone(&*r))
                .ok_or(StatusCode::NOT_FOUND)?;
            (id.to_owned(), handle)
        } else {
            create_connection(&state)?
        };

    {
        let mut w = handle.writer.lock().await;
        w.write_all(body.as_bytes())
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        w.write_all(b"\n")
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    handle.touch();

    let mut rx = handle.output_tx.subscribe();
    let stream = async_stream::stream! {
        while let Ok(line) = rx.recv().await {
            yield Ok::<_, std::convert::Infallible>(
                Event::default().event("message").data(line)
            );
        }
    };

    let sse = Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    );

    let mut response = sse.into_response();
    response.headers_mut().insert(
        "acp-session-id",
        session_id
            .parse()
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    );
    Ok(response)
}

/// `GET /acp` — SSE notification stream for an existing session (reconnect).
///
/// Requires `Acp-Session-Id` header with a valid UUID value.
///
/// # Errors
///
/// Returns `400 Bad Request` if `Acp-Session-Id` header is missing or not a valid UUID.
/// Returns `404 Not Found` if the session ID is not found.
#[cfg(feature = "acp-http")]
pub async fn get_handler(
    State(state): State<AcpHttpState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    let id = headers
        .get("acp-session-id")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::BAD_REQUEST)?;

    uuid::Uuid::parse_str(id).map_err(|_| StatusCode::BAD_REQUEST)?;

    let handle = state
        .connections
        .get(id)
        .map(|r| Arc::clone(&*r))
        .ok_or(StatusCode::NOT_FOUND)?;

    let mut rx = handle.output_tx.subscribe();
    let stream = async_stream::stream! {
        while let Ok(line) = rx.recv().await {
            yield Ok::<_, std::convert::Infallible>(
                Event::default().event("message").data(line)
            );
        }
    };

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    ))
}

/// `GET /sessions` — list all persisted ACP sessions ordered by last activity.
///
/// # Errors
///
/// Returns `503 Service Unavailable` if no `SQLite` store is configured.
/// Returns `500 Internal Server Error` if the database query fails.
#[cfg(feature = "acp-http")]
pub async fn list_sessions_handler(
    State(state): State<AcpHttpState>,
) -> Result<impl IntoResponse, StatusCode> {
    let store = state
        .store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let sessions = store
        .list_acp_sessions(state.server_config.max_history)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "failed to list ACP sessions");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let summaries: Vec<SessionSummary> = sessions.into_iter().map(SessionSummary::from).collect();
    Ok(Json(summaries))
}

/// `GET /sessions/{id}/messages` — retrieve all events for a persisted ACP session.
///
/// # Errors
///
/// Returns `503 Service Unavailable` if no `SQLite` store is configured.
/// Returns `404 Not Found` if the session does not exist.
/// Returns `500 Internal Server Error` if the database query fails.
#[cfg(feature = "acp-http")]
pub async fn session_messages_handler(
    State(state): State<AcpHttpState>,
    Path(session_id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    uuid::Uuid::parse_str(&session_id).map_err(|_| StatusCode::BAD_REQUEST)?;

    let store = state
        .store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let exists = store.acp_session_exists(&session_id).await.map_err(|e| {
        tracing::warn!(error = %e, "failed to check ACP session existence");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    if !exists {
        return Err(StatusCode::NOT_FOUND);
    }

    let events = store.load_acp_events(&session_id).await.map_err(|e| {
        tracing::warn!(error = %e, "failed to load ACP session events");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let dtos: Vec<SessionEventDto> = events
        .into_iter()
        .map(|e| SessionEventDto {
            event_type: e.event_type,
            payload: e.payload,
            created_at: e.created_at,
        })
        .collect();
    Ok(Json(dtos))
}
