// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! CRUD REST endpoints for ACP session management.
//!
//! These handlers complement the existing `GET /sessions` and
//! `GET /sessions/{id}/messages` endpoints with full lifecycle control:
//! create, inspect, update, and delete individual sessions.

#![cfg(feature = "acp-http")]

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use zeph_memory::store::AcpSessionInfo;

use crate::transport::http::AcpHttpState;

// ── Request / response types ──────────────────────────────────────────────────

/// Status of an ACP session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// The session has an active HTTP/WebSocket connection.
    Running,
    /// The session exists but has no active connection (history-only).
    Idle,
    /// The session was explicitly stopped or reaped.
    Stopped,
    /// The session encountered an unrecoverable error.
    Error,
}

/// Full session details returned by `POST /sessions` and `GET /sessions/{id}`.
#[derive(Debug, Serialize)]
pub struct SessionInfo {
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
    /// Current session lifecycle status.
    pub status: SessionStatus,
    /// Working directory reported for this session, if known.
    pub working_dir: Option<PathBuf>,
}

impl SessionInfo {
    fn from_store(
        info: AcpSessionInfo,
        status: SessionStatus,
        working_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            id: info.id,
            title: info.title,
            created_at: info.created_at,
            updated_at: info.updated_at,
            message_count: info.message_count,
            status,
            working_dir,
        }
    }
}

/// Request body for `POST /sessions`.
#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    /// Optional working directory for the new session.
    ///
    /// When provided, the path is validated against the `additional_directories`
    /// allowlist configured for the ACP server. Requests with a path outside the
    /// allowlist are rejected with `403 Forbidden`.
    pub working_dir: Option<PathBuf>,
    /// Optional model override (`"provider:model"` format).
    pub model: Option<String>,
    /// Optional session mode identifier.
    pub mode: Option<String>,
}

/// Request body for `PATCH /sessions/{id}`.
#[derive(Debug, Deserialize)]
pub struct UpdateSessionRequest {
    /// New title for the session.
    pub title: Option<String>,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Determine `SessionStatus` for a session ID by checking active connections.
fn resolve_status(state: &AcpHttpState, id: &str) -> SessionStatus {
    if state.connections.contains_key(id) {
        SessionStatus::Running
    } else {
        SessionStatus::Idle
    }
}

/// Validate `working_dir` against the `additional_directories` allowlist.
///
/// Returns `Ok(())` when the path is covered by the allowlist or when the
/// allowlist is empty and `working_dir` is `None`.
///
/// # Errors
///
/// Returns `Err(StatusCode::FORBIDDEN)` when `working_dir` is present but
/// not covered by the allowlist.
async fn validate_working_dir(
    state: &AcpHttpState,
    working_dir: Option<&PathBuf>,
) -> Result<(), StatusCode> {
    let Some(dir) = working_dir else {
        return Ok(());
    };

    let allowlist = &state.server_config.additional_directories;

    if allowlist.is_empty() {
        tracing::warn!(
            path = %dir.display(),
            "POST /sessions rejected: working_dir provided but additional_directories allowlist is empty"
        );
        return Err(StatusCode::FORBIDDEN);
    }

    // Canonicalize once; if it fails (path does not exist) reject immediately.
    let canonical = tokio::fs::canonicalize(dir).await.map_err(|e| {
        tracing::warn!(path = %dir.display(), error = %e, "cannot canonicalize working_dir");
        StatusCode::FORBIDDEN
    })?;

    let allowed = allowlist
        .iter()
        .any(|entry| canonical.starts_with(entry.as_path()));

    if !allowed {
        tracing::warn!(
            path = %canonical.display(),
            "POST /sessions rejected: working_dir not in additional_directories allowlist"
        );
        return Err(StatusCode::FORBIDDEN);
    }

    Ok(())
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// `POST /sessions` — create a new ACP session record.
///
/// Persists the session in the `SQLite` store and returns the full `SessionInfo`.
/// `working_dir`, `model`, and `mode` are accepted but advisory for history
/// callers (the actual agent loop is spawned lazily on the first prompt).
///
/// # Errors
///
/// Returns `403 Forbidden` when `working_dir` is outside the allowlist.
/// Returns `503 Service Unavailable` when no `SQLite` store is configured.
/// Returns `500 Internal Server Error` if the database write fails.
pub async fn create_session_handler(
    State(state): State<AcpHttpState>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    if !state.ready.load(std::sync::atomic::Ordering::Acquire) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    validate_working_dir(&state, req.working_dir.as_ref()).await?;

    let store = state
        .store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let session_id = uuid::Uuid::new_v4().to_string();

    store.create_acp_session(&session_id).await.map_err(|e| {
        tracing::warn!(error = %e, "failed to create ACP session");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let info = store.get_acp_session_info(&session_id).await.map_err(|e| {
        tracing::warn!(error = %e, "failed to retrieve created ACP session");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let Some(info) = info else {
        tracing::warn!(session_id, "created ACP session not found immediately");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };

    let body = SessionInfo::from_store(info, SessionStatus::Idle, req.working_dir);
    Ok((StatusCode::CREATED, Json(body)))
}

/// `GET /sessions/{id}` — retrieve full details for a single ACP session.
///
/// # Errors
///
/// Returns `400 Bad Request` if `{id}` is not a valid UUID.
/// Returns `404 Not Found` if the session does not exist.
/// Returns `503 Service Unavailable` when no `SQLite` store is configured.
/// Returns `500 Internal Server Error` if the database query fails.
pub async fn get_session_handler(
    State(state): State<AcpHttpState>,
    Path(session_id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    if !state.ready.load(std::sync::atomic::Ordering::Acquire) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    uuid::Uuid::parse_str(&session_id).map_err(|_| StatusCode::BAD_REQUEST)?;

    let store = state
        .store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let info = store
        .get_acp_session_info(&session_id)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "failed to get ACP session info");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    let status = resolve_status(&state, &session_id);
    Ok(Json(SessionInfo::from_store(info, status, None)))
}

/// `PATCH /sessions/{id}` — update mutable metadata for an existing session.
///
/// Currently supports renaming the session title.
///
/// # Errors
///
/// Returns `400 Bad Request` if `{id}` is not a valid UUID.
/// Returns `404 Not Found` if the session does not exist.
/// Returns `503 Service Unavailable` when no `SQLite` store is configured.
/// Returns `500 Internal Server Error` if the database write or subsequent query fails.
pub async fn update_session_handler(
    State(state): State<AcpHttpState>,
    Path(session_id): Path<String>,
    Json(req): Json<UpdateSessionRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    if !state.ready.load(std::sync::atomic::Ordering::Acquire) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
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

    if let Some(title) = req.title {
        store
            .update_session_title(&session_id, &title)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "failed to update ACP session title");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
    }

    let info = store
        .get_acp_session_info(&session_id)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "failed to fetch updated ACP session info");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    let status = resolve_status(&state, &session_id);
    Ok(Json(SessionInfo::from_store(info, status, None)))
}

/// `DELETE /sessions/{id}` — remove an ACP session and its event history.
///
/// Active in-memory connections for the session are also dropped.
/// Returns `204 No Content` on success.
///
/// Single-tenant only: any authenticated caller can delete any session.
///
/// # Errors
///
/// Returns `400 Bad Request` if `{id}` is not a valid UUID.
/// Returns `404 Not Found` if the session does not exist.
/// Returns `503 Service Unavailable` when no `SQLite` store is configured.
/// Returns `500 Internal Server Error` if the database delete fails.
pub async fn delete_session_handler(
    State(state): State<AcpHttpState>,
    Path(session_id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    if !state.ready.load(std::sync::atomic::Ordering::Acquire) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
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

    store.delete_acp_session(&session_id).await.map_err(|e| {
        tracing::warn!(error = %e, "failed to delete ACP session");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Drop the active connection if it exists.
    state.connections.remove(&session_id);

    Ok(StatusCode::NO_CONTENT)
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::{delete, get, patch, post};
    use tokio::sync::{Mutex, broadcast};
    use tower::ServiceExt as _;
    use zeph_core::channel::LoopbackChannel;

    use super::*;
    use crate::agent::{AcpContext, SendAgentSpawner, SessionContext};
    use crate::transport::http::{AcpHttpState, ConnectionHandle};
    use crate::transport::{AcpServerConfig, SharedAvailableModels};

    fn shared_models() -> SharedAvailableModels {
        Arc::new(parking_lot::RwLock::new(vec![]))
    }

    fn noop_spawner() -> SendAgentSpawner {
        Arc::new(
            |_ch: LoopbackChannel, _ctx: Option<AcpContext>, _sess: SessionContext| {
                Box::pin(async {})
                    as Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>
            },
        )
    }

    fn base_config() -> AcpServerConfig {
        AcpServerConfig {
            agent_name: "test".into(),
            agent_version: "0.0.1".into(),
            max_sessions: 4,
            session_idle_timeout_secs: 1800,
            available_models: shared_models(),
            ..AcpServerConfig::default()
        }
    }

    fn build_router(state: AcpHttpState) -> Router {
        Router::new()
            .route("/sessions", post(create_session_handler))
            .route("/sessions/{id}", get(get_session_handler))
            .route("/sessions/{id}", patch(update_session_handler))
            .route("/sessions/{id}", delete(delete_session_handler))
            .with_state(state)
    }

    // Helper: a state with no SQLite store configured (store = None).
    fn state_no_store() -> AcpHttpState {
        AcpHttpState::new(noop_spawner(), base_config()).with_ready(true)
    }

    #[tokio::test]
    async fn create_session_returns_503_without_store() {
        let app = build_router(state_no_store());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn get_session_returns_503_without_store() {
        let app = build_router(state_no_store());

        let id = uuid::Uuid::new_v4();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/sessions/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn get_session_rejects_invalid_uuid() {
        let app = build_router(state_no_store());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/sessions/not-a-uuid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn patch_session_rejects_invalid_uuid() {
        let app = build_router(state_no_store());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/sessions/not-a-uuid")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn delete_session_rejects_invalid_uuid() {
        let app = build_router(state_no_store());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/sessions/not-a-uuid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_session_rejects_working_dir_when_allowlist_empty() {
        let app = build_router(state_no_store());

        let body = serde_json::json!({ "working_dir": "/tmp/test" });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        // 503 because no store, but working_dir validation hits first → 403.
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn create_session_accepts_request_without_working_dir_when_allowlist_empty() {
        // No working_dir → passes allowlist check, then fails with 503 (no store).
        let app = build_router(state_no_store());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn session_status_running_when_connection_exists() {
        let state = state_no_store();
        // Simulate an active connection by inserting a dummy handle.
        let (_, writer) = tokio::io::duplex(64);
        let (tx, _) = broadcast::channel(4);
        let session_id = uuid::Uuid::new_v4().to_string();
        let handle = Arc::new(ConnectionHandle {
            writer: Arc::new(Mutex::new(writer)),
            output_tx: tx,
            last_activity: AtomicU64::new(0),
            idle_timeout_secs: 1800,
        });
        state.connections.insert(session_id.clone(), handle);

        assert_eq!(resolve_status(&state, &session_id), SessionStatus::Running);
    }

    #[tokio::test]
    async fn session_status_idle_when_no_connection() {
        let state = state_no_store();
        let session_id = uuid::Uuid::new_v4().to_string();
        assert_eq!(resolve_status(&state, &session_id), SessionStatus::Idle);
    }
}
