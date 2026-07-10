// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! HTTP + SSE transport handlers for the ACP server.
//!
//! Requires feature `acp-http`. All public items in this module are re-exported
//! from the crate root when the feature is active.
//!
//! # Session lifecycle
//!
//! ```text
//! POST /acp  (no Acp-Session-Id)
//!   → create_connection()  → new AgentSideConnection thread
//!   → returns Acp-Session-Id + SSE stream
//!
//! POST /acp  (Acp-Session-Id: <id>)
//!   → route to existing ConnectionHandle
//!   → subscribe to broadcast channel → SSE stream
//!
//! GET /acp   (Acp-Session-Id: <id>)
//!   → reconnect to SSE stream (e.g. after network drop)
//!
//! GET /acp/ws
//!   → WebSocket upgrade → duplex framing over single connection
//! ```

#[cfg(feature = "acp-http")]
use std::sync::Arc;
#[cfg(feature = "acp-http")]
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
#[cfg(feature = "acp-http")]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
use zeph_common::task_supervisor::{RestartPolicy, TaskDescriptor, TaskSupervisor};

#[cfg(feature = "acp-http")]
use axum::Json;
#[cfg(feature = "acp-http")]
use axum::extract::Path;
#[cfg(feature = "acp-http")]
use serde::Serialize;
#[cfg(feature = "acp-http")]
use zeph_memory::store::{AcpSessionInfo, SqliteStore};

#[cfg(feature = "acp-http")]
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[cfg(feature = "acp-http")]
use crate::agent::SendAgentSpawner;
#[cfg(feature = "acp-http")]
use crate::transport::AcpServerConfig;

#[cfg(feature = "acp-http")]
const BRIDGE_BUFFER_SIZE: usize = 64 * 1024;

// macOS default is 512 KiB; Linux/Windows are 1–2 MiB. 8 MiB matches the
// Linux main-thread default and provides headroom for deeply nested agent futures.
#[cfg(feature = "acp-http")]
const ACP_AGENT_STACK_SIZE: usize = 8 * 1024 * 1024;

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

/// Serializable session metadata returned by `GET /sessions`.
#[cfg(feature = "acp-http")]
#[cfg_attr(docsrs, doc(cfg(feature = "acp-http")))]
#[derive(Serialize)]
pub struct SessionSummary {
    /// ACP session UUID.
    pub id: String,
    /// Auto-generated session title, if available.
    pub title: Option<String>,
    /// ISO 8601 timestamp of session creation.
    pub created_at: String,
    /// ISO 8601 timestamp of the last session update.
    pub updated_at: String,
    /// Total number of persisted events in this session.
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

/// A single persisted ACP event returned by `GET /sessions/{id}/messages`.
#[cfg(feature = "acp-http")]
#[cfg_attr(docsrs, doc(cfg(feature = "acp-http")))]
#[derive(Serialize)]
pub struct SessionEventDto {
    /// Event type tag (e.g. `"user_message"`, `"agent_message"`, `"tool_call"`).
    pub event_type: String,
    /// JSON-encoded event payload.
    pub payload: String,
    /// ISO 8601 timestamp of when the event was persisted.
    pub created_at: String,
}

/// Liveness payload returned by `GET /health`.
#[cfg(feature = "acp-http")]
#[cfg_attr(docsrs, doc(cfg(feature = "acp-http")))]
#[derive(Serialize)]
pub struct HealthStatus {
    /// `"ok"` when the server is ready, `"starting"` otherwise.
    pub status: &'static str,
    /// Semver version of the running agent.
    pub version: String,
    /// Seconds elapsed since the server started.
    pub uptime_secs: u64,
}

/// Shared axum `State` for the HTTP+SSE and WebSocket transport.
///
/// Holds all mutable server state behind `Arc` so it can be cheaply cloned
/// into each request handler. Use [`AcpHttpState::new`] to construct, then
/// optionally attach a `SQLite` store with [`AcpHttpState::with_store`].
///
/// # Examples
///
/// ```rust,no_run
/// # use std::sync::Arc;
/// # use parking_lot::RwLock;
/// # use zeph_acp::{AgentSpawner, AcpServerConfig};
/// # #[cfg(feature = "acp-http")]
/// # {
/// use zeph_acp::AcpHttpState;
///
/// let spawner: AgentSpawner = Arc::new(|ch, ctx, sess| Box::pin(async move { drop((ch, ctx, sess)); }));
/// let config = AcpServerConfig { agent_name: "zeph".to_owned(), ..AcpServerConfig::default() };
/// let state = AcpHttpState::new(spawner, config);
/// state.mark_ready();
/// # }
/// ```
#[cfg(feature = "acp-http")]
#[cfg_attr(docsrs, doc(cfg(feature = "acp-http")))]
#[derive(Clone)]
pub struct AcpHttpState {
    pub(crate) connections: Arc<DashMap<String, Arc<ConnectionHandle>>>,
    /// Agent spawner used when creating new HTTP/WebSocket connections.
    pub spawner: SendAgentSpawner,
    /// Server configuration shared across all connections.
    pub server_config: Arc<AcpServerConfig>,
    /// Atomic counter for active WebSocket sessions.
    ///
    /// Used to atomically reserve a slot before the upgrade handshake, eliminating TOCTOU
    /// between the capacity check and the actual `DashMap` insertion.
    pub(crate) active_ws: Arc<AtomicUsize>,
    /// Optional `SQLite` store for the session history REST endpoints.
    pub store: Option<Arc<SqliteStore>>,
    pub(crate) started_at: Instant,
    pub(crate) ready: Arc<AtomicBool>,
    /// Supervisor for long-lived HTTP-level background tasks (reaper).
    task_supervisor: Arc<TaskSupervisor>,
}

#[cfg(feature = "acp-http")]
impl AcpHttpState {
    /// Create a new HTTP state with the given spawner and server configuration.
    ///
    /// The server starts in a "not ready" state. Call [`mark_ready`] after all
    /// initialization (e.g. vault unlock, MCP connect) is complete so that
    /// `GET /health` returns `200 OK`.
    ///
    /// [`mark_ready`]: AcpHttpState::mark_ready
    pub fn new(spawner: SendAgentSpawner, server_config: AcpServerConfig) -> Self {
        let reaper_cancel = tokio_util::sync::CancellationToken::new();
        let task_supervisor = Arc::new(TaskSupervisor::new(reaper_cancel));
        Self {
            connections: Arc::new(DashMap::new()),
            spawner,
            server_config: Arc::new(server_config),
            active_ws: Arc::new(AtomicUsize::new(0)),
            store: None,
            started_at: Instant::now(),
            ready: Arc::new(AtomicBool::new(false)),
            task_supervisor,
        }
    }

    /// Attach a `SQLite` store for the session history REST endpoints.
    ///
    /// Required for `GET /sessions` and `GET /sessions/{id}/messages` to function.
    #[must_use]
    pub fn with_store(mut self, store: SqliteStore) -> Self {
        self.store = Some(Arc::new(store));
        self
    }

    /// Set the initial readiness state (builder-style).
    #[must_use]
    pub fn with_ready(self, ready: bool) -> Self {
        self.ready.store(ready, Ordering::Release);
        self
    }

    /// Mark the server as ready to serve ACP requests.
    ///
    /// After this call, `GET /health` returns `200 OK` and all `/acp` endpoints
    /// accept new connections.
    pub fn mark_ready(&self) {
        self.ready.store(true, Ordering::Release);
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
        self.task_supervisor.spawn(TaskDescriptor {
            name: "acp_http_reaper",
            restart: RestartPolicy::Restart {
                max: 0,
                base_delay: Duration::from_secs(1),
            },
            factory: move || {
                let connections = Arc::clone(&connections);
                async move {
                    let mut interval = tokio::time::interval(Duration::from_mins(1));
                    loop {
                        interval.tick().await;
                        connections.retain(|_, handle| !handle.is_expired());
                    }
                }
            },
        });
    }
}

/// `GET /health` — public readiness probe for ACP HTTP transport.
///
/// Returns `503 Service Unavailable` until the ACP server marks itself ready.
#[cfg(feature = "acp-http")]
#[tracing::instrument(skip_all, name = "acp.http.health")]
pub async fn health_handler(State(state): State<AcpHttpState>) -> impl IntoResponse {
    let ready = state.ready.load(Ordering::Acquire);
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = HealthStatus {
        status: if ready { "ok" } else { "starting" },
        version: state.server_config.agent_version.clone(),
        uptime_secs: state.started_at.elapsed().as_secs(),
    };
    (status, Json(body))
}

/// Derive the connection's `owner_key` (#5868) from the auth layer's matched identity.
///
/// `token_identity` is `Some` only when [`BearerAuthLayer`](super::auth::BearerAuthLayer)
/// ran and matched a configured client — i.e. `auth_clients` is non-empty. When
/// `auth_clients` is empty (unauthenticated deployment) or no layer ran, falls back to
/// [`crate::transport::OWNER_KEY_LOCAL`], the same bucket stdio uses.
#[cfg(feature = "acp-http")]
pub(crate) fn derive_owner_key(token_identity: Option<&super::auth::TokenIdentity>) -> String {
    token_identity.map_or_else(
        || crate::transport::OWNER_KEY_LOCAL.to_owned(),
        |t| t.0.clone(),
    )
}

/// Spawn an in-process ACP agent connection on a dedicated thread.
///
/// Agent futures are `!Send` (they call `spawn_local` internally), so each connection
/// runs on its own current-thread Tokio runtime inside a `LocalSet`. Returns two
/// `DuplexStream`s: `(reader, writer)` from the caller's perspective.
///
/// # Errors
///
/// Returns an [`std::io::Error`] if the OS refuses to create the thread.
#[cfg(feature = "acp-http")]
pub(crate) fn spawn_agent_connection(
    spawner: crate::agent::SendAgentSpawner,
    server_config: AcpServerConfig,
    owner_key: String,
) -> std::io::Result<(DuplexStream, DuplexStream)> {
    let (client_w, agent_r) = tokio::io::duplex(BRIDGE_BUFFER_SIZE);
    let (agent_w, client_r) = tokio::io::duplex(BRIDGE_BUFFER_SIZE);
    std::thread::Builder::new()
        .stack_size(ACP_AGENT_STACK_SIZE)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio current-thread runtime for ACP agent");
            let local = tokio::task::LocalSet::new();
            rt.block_on(local.run_until(async move {
                let writer = agent_w.compat_write();
                let reader = agent_r.compat();
                if let Err(e) = crate::transport::stdio::serve_connection(
                    spawner,
                    server_config,
                    writer,
                    reader,
                    owner_key,
                )
                .await
                {
                    tracing::error!("ACP agent connection error: {e}");
                }
            }));
        })?;
    Ok((client_r, client_w))
}

/// Create a new HTTP+SSE connection.
///
/// # Errors
///
/// Returns `503 Service Unavailable` when `max_sessions` is already reached or when
/// the OS refuses to spawn the agent thread.
#[cfg(feature = "acp-http")]
pub(crate) fn create_connection(
    state: &AcpHttpState,
    owner_key: &str,
) -> Result<(String, Arc<ConnectionHandle>), StatusCode> {
    if state.connections.len() >= state.server_config.max_sessions {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    let (reader, writer) = spawn_agent_connection(
        state.spawner.clone(),
        (*state.server_config).clone(),
        owner_key.to_owned(),
    )
    .map_err(|e| {
        tracing::error!("failed to spawn ACP agent thread: {e}");
        StatusCode::SERVICE_UNAVAILABLE
    })?;

    let (tx, _) = broadcast::channel(256);
    let tx2 = tx.clone();
    // EXEMPT(#5144): per-connection SSE reader pump; self-terminating when the agent
    // thread closes the pipe. Per-connection naming would flood the registry.
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
#[tracing::instrument(skip_all, name = "acp.http.post")]
pub async fn post_handler(
    State(state): State<AcpHttpState>,
    token_identity: Option<axum::extract::Extension<super::auth::TokenIdentity>>,
    headers: HeaderMap,
    body: String,
) -> Result<impl IntoResponse, StatusCode> {
    if !state.ready.load(Ordering::Acquire) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
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
            let owner_key = derive_owner_key(token_identity.as_ref().map(|e| &e.0));
            create_connection(&state, &owner_key)?
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
#[tracing::instrument(skip_all, name = "acp.http.get")]
pub async fn get_handler(
    State(state): State<AcpHttpState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    if !state.ready.load(Ordering::Acquire) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
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
#[tracing::instrument(skip_all, name = "acp.http.list_sessions")]
pub async fn list_sessions_handler(
    State(state): State<AcpHttpState>,
    token_identity: Option<axum::extract::Extension<super::auth::TokenIdentity>>,
) -> Result<impl IntoResponse, StatusCode> {
    if !state.ready.load(Ordering::Acquire) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let store = state
        .store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let owner_key = derive_owner_key(token_identity.as_ref().map(|e| &e.0));
    let sessions = store
        .list_acp_sessions_for_owner(state.server_config.max_history, &owner_key)
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
/// Reads `session_id`'s durable JSONL event log (`state.server_config.session_data_dir`) instead
/// of the legacy `acp_session_events` table (spec-068 §12.3 / D-2), which the P1 write cutover
/// leaves permanently empty for every post-cutover session — same bug class and fix shape as
/// `do_load_session`/`do_list_sessions` in `crates/zeph-acp/src/agent/mod.rs` (S1). Returns an
/// empty array (not an error) when `[session] data_dir` isn't configured or the log can't be
/// read — matching `do_load_session`'s replay tolerance of missing durable history.
///
/// # Errors
///
/// Returns `503 Service Unavailable` if no `SQLite` store is configured.
/// Returns `404 Not Found` if the session does not exist.
/// Returns `500 Internal Server Error` if the database query fails.
#[cfg(feature = "acp-http")]
#[tracing::instrument(skip_all, name = "acp.http.session_messages", fields(session_id = %session_id))]
pub async fn session_messages_handler(
    State(state): State<AcpHttpState>,
    token_identity: Option<axum::extract::Extension<super::auth::TokenIdentity>>,
    Path(session_id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    if !state.ready.load(Ordering::Acquire) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    uuid::Uuid::parse_str(&session_id).map_err(|_| StatusCode::BAD_REQUEST)?;

    let store = state
        .store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    // Read-only gate (#5868): does not claim a legacy NULL row.
    let owner_key = derive_owner_key(token_identity.as_ref().map(|e| &e.0));
    let accessible = store
        .acp_session_accessible_for_owner(&session_id, &owner_key)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "failed to check ACP session existence");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    if !accessible {
        return Err(StatusCode::NOT_FOUND);
    }

    let dtos = load_session_event_dtos(&state, &session_id).await;
    Ok(Json(dtos))
}

/// Read `session_id`'s durable event log and map each event to a [`SessionEventDto`] row.
///
/// Soft-fails to an empty `Vec` (logging a warning) when `session_data_dir` is unset or the log
/// can't be opened/read — mirrors `crates/zeph-acp/src/agent/mod.rs`'s
/// `load_session_replay_events` tolerance for the same scenario.
#[cfg(feature = "acp-http")]
async fn load_session_event_dtos(state: &AcpHttpState, session_id: &str) -> Vec<SessionEventDto> {
    let Some(ref data_dir) = state.server_config.session_data_dir else {
        return Vec::new();
    };
    let session_path = zeph_session::session_dir(data_dir, session_id);
    let log = match zeph_session::SessionEventLog::open(&session_path).await {
        Ok(log) => log,
        Err(e) => {
            tracing::warn!(error = %e, "failed to open session event log for HTTP messages endpoint");
            return Vec::new();
        }
    };
    let events = match log.read_all().await {
        Ok(events) => events,
        Err(e) => {
            tracing::warn!(error = %e, "failed to read session event log for HTTP messages endpoint");
            return Vec::new();
        }
    };
    events
        .into_iter()
        .flat_map(session_event_envelope_to_dtos)
        .collect()
}

/// Map one durable [`zeph_session::SessionEventEnvelope`] to the [`SessionEventDto`] row(s) it
/// surfaces as. `SessionStarted`/`ForkPoint`/`Condensation`/`Compaction`/`ModelChanged`/
/// `SessionEnded` are session-log bookkeeping, not turn content — they produce no row, matching
/// `session_event_to_updates`'s equivalent bookkeeping carve-out for the ACP JSON-RPC path.
#[cfg(feature = "acp-http")]
fn session_event_envelope_to_dtos(
    envelope: zeph_session::SessionEventEnvelope,
) -> Vec<SessionEventDto> {
    let created_at = chrono::DateTime::from_timestamp_millis(envelope.ts_ms).map_or_else(
        || envelope.ts_ms.to_string(),
        |dt| dt.format("%Y-%m-%d %H:%M:%S").to_string(),
    );
    match envelope.kind {
        zeph_session::SessionEvent::UserMessage { text, .. } => vec![SessionEventDto {
            event_type: "user_message".to_owned(),
            payload: text,
            created_at,
        }],
        zeph_session::SessionEvent::AssistantMessage { parts } => parts
            .into_iter()
            .filter_map(|part| match part {
                zeph_llm::provider::MessagePart::ToolUse { id, name, input } => {
                    serde_json::to_string(&serde_json::json!({
                        "id": id,
                        "name": name,
                        "input": input,
                    }))
                    .ok()
                    .map(|payload| SessionEventDto {
                        event_type: "tool_call".to_owned(),
                        payload,
                        created_at: created_at.clone(),
                    })
                }
                other => other.as_plain_text().map(|text| SessionEventDto {
                    event_type: "agent_message".to_owned(),
                    payload: text.to_owned(),
                    created_at: created_at.clone(),
                }),
            })
            .collect(),
        zeph_session::SessionEvent::ToolCall { id, name, input } => {
            serde_json::to_string(&serde_json::json!({ "id": id, "name": name, "input": input }))
                .ok()
                .map(|payload| {
                    vec![SessionEventDto {
                        event_type: "tool_call".to_owned(),
                        payload,
                        created_at,
                    }]
                })
                .unwrap_or_default()
        }
        zeph_session::SessionEvent::ToolResult { id, output, .. } => {
            vec![SessionEventDto {
                event_type: "tool_result".to_owned(),
                payload: serde_json::json!({ "id": id, "output": output }).to_string(),
                created_at,
            }]
        }
        zeph_session::SessionEvent::SessionStarted { .. }
        | zeph_session::SessionEvent::ForkPoint { .. }
        | zeph_session::SessionEvent::Condensation { .. }
        | zeph_session::SessionEvent::Compaction { .. }
        | zeph_session::SessionEvent::ModelChanged { .. }
        | zeph_session::SessionEvent::SessionEnded { .. } => Vec::new(),
    }
}
