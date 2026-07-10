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
#[non_exhaustive]
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
    token_identity: Option<axum::extract::Extension<super::auth::TokenIdentity>>,
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

    let owner_key = super::http::derive_owner_key(token_identity.as_ref().map(|e| &e.0));
    let session_id = uuid::Uuid::new_v4().to_string();

    store
        .create_acp_session(&session_id, Some(&owner_key))
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "failed to create ACP session");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let info = store
        .get_acp_session_info_for_owner(&session_id, &owner_key)
        .await
        .map_err(|e| {
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
    token_identity: Option<axum::extract::Extension<super::auth::TokenIdentity>>,
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

    let owner_key = super::http::derive_owner_key(token_identity.as_ref().map(|e| &e.0));
    let info = store
        .get_acp_session_info_for_owner(&session_id, &owner_key)
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
/// Returns `400 Bad Request` if `{id}` is not a valid UUID or the title exceeds
/// `AcpServerConfig::title_max_chars`.
/// Returns `404 Not Found` if the session does not exist.
/// Returns `503 Service Unavailable` when no `SQLite` store is configured.
/// Returns `500 Internal Server Error` if the database write or subsequent query fails.
pub async fn update_session_handler(
    State(state): State<AcpHttpState>,
    token_identity: Option<axum::extract::Extension<super::auth::TokenIdentity>>,
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

    if let Some(ref title) = req.title
        && title.chars().count() > state.server_config.title_max_chars
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    let owner_key = super::http::derive_owner_key(token_identity.as_ref().map(|e| &e.0));
    let found = if let Some(title) = req.title {
        store
            .update_session_title_for_owner(&session_id, &title, &owner_key)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "failed to update ACP session title");
                StatusCode::INTERNAL_SERVER_ERROR
            })?
    } else {
        store
            .acp_session_accessible_for_owner(&session_id, &owner_key)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "failed to check ACP session existence");
                StatusCode::INTERNAL_SERVER_ERROR
            })?
    };
    if !found {
        return Err(StatusCode::NOT_FOUND);
    }

    let info = store
        .get_acp_session_info_for_owner(&session_id, &owner_key)
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
/// Scoped to the caller's owner identity (#5868): only sessions owned by the caller, or
/// unowned legacy/non-ACP rows, can be deleted — a session owned by a different client is
/// reported as `404 Not Found`, identical to a nonexistent id.
///
/// # Errors
///
/// Returns `400 Bad Request` if `{id}` is not a valid UUID.
/// Returns `404 Not Found` if the session does not exist.
/// Returns `503 Service Unavailable` when no `SQLite` store is configured.
/// Returns `500 Internal Server Error` if the database delete fails.
pub async fn delete_session_handler(
    State(state): State<AcpHttpState>,
    token_identity: Option<axum::extract::Extension<super::auth::TokenIdentity>>,
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

    let owner_key = super::http::derive_owner_key(token_identity.as_ref().map(|e| &e.0));
    let deleted = store
        .delete_acp_session_for_owner(&session_id, &owner_key)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "failed to delete ACP session");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    if !deleted {
        return Err(StatusCode::NOT_FOUND);
    }

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

    // ── Helpers for store-backed tests ────────────────────────────────────────

    async fn state_with_store() -> AcpHttpState {
        let store = zeph_memory::store::SqliteStore::new(":memory:")
            .await
            .expect("SqliteStore::new");
        AcpHttpState::new(noop_spawner(), base_config())
            .with_store(store)
            .with_ready(true)
    }

    async fn state_with_store_and_limit(title_max_chars: usize) -> AcpHttpState {
        let store = zeph_memory::store::SqliteStore::new(":memory:")
            .await
            .expect("SqliteStore::new");
        let mut cfg = base_config();
        cfg.title_max_chars = title_max_chars;
        AcpHttpState::new(noop_spawner(), cfg)
            .with_store(store)
            .with_ready(true)
    }

    // ── PATCH title-length validation (#4260) ─────────────────────────────────

    #[tokio::test]
    async fn patch_title_over_limit_returns_400() {
        let limit = 10;
        let state = state_with_store_and_limit(limit).await;
        let app = build_router(state);

        let id = uuid::Uuid::new_v4();
        let title = "a".repeat(limit + 1);
        let body = serde_json::json!({ "title": title });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/sessions/{id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn patch_title_exactly_at_limit_returns_404_for_missing_session() {
        // Title length is valid; session does not exist → 404 (not 400 or 500).
        let limit = 10;
        let state = state_with_store_and_limit(limit).await;
        let app = build_router(state);

        let id = uuid::Uuid::new_v4();
        let title = "a".repeat(limit); // exactly at limit
        let body = serde_json::json!({ "title": title });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/sessions/{id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ── DELETE / PATCH on non-existent session (#4262) ────────────────────────

    #[tokio::test]
    async fn delete_nonexistent_session_returns_404() {
        let state = state_with_store().await;
        let app = build_router(state);

        let id = uuid::Uuid::new_v4();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/sessions/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn patch_nonexistent_session_returns_404() {
        let state = state_with_store().await;
        let app = build_router(state);

        let id = uuid::Uuid::new_v4();
        let body = serde_json::json!({ "title": "new name" });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/sessions/{id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ── Happy paths ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn patch_existing_session_returns_200_with_updated_title() {
        let state = state_with_store().await;
        // Create the session directly via the store before routing.
        let session_id = uuid::Uuid::new_v4().to_string();
        state
            .store
            .as_ref()
            .unwrap()
            .create_acp_session(&session_id, Some(crate::transport::OWNER_KEY_LOCAL))
            .await
            .unwrap();

        let app = build_router(state);

        let body = serde_json::json!({ "title": "renamed" });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/sessions/{session_id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let info: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(info["title"], "renamed");
    }

    #[tokio::test]
    async fn delete_existing_session_returns_204() {
        let state = state_with_store().await;
        let session_id = uuid::Uuid::new_v4().to_string();
        state
            .store
            .as_ref()
            .unwrap()
            .create_acp_session(&session_id, Some(crate::transport::OWNER_KEY_LOCAL))
            .await
            .unwrap();

        let app = build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/sessions/{session_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    // ── Cross-owner scoping (#5868) ────────────────────────────────────────────

    fn two_clients() -> Vec<crate::transport::AcpClientToken> {
        vec![
            crate::transport::AcpClientToken {
                id: "alice".into(),
                token: "token-a".into(),
            },
            crate::transport::AcpClientToken {
                id: "bob".into(),
                token: "token-b".into(),
            },
        ]
    }

    fn build_router_with_auth(
        state: AcpHttpState,
        clients: Vec<crate::transport::AcpClientToken>,
    ) -> Router {
        build_router(state).layer(crate::transport::auth::BearerAuthLayer::new(clients))
    }

    fn authed(method: &str, uri: String, bearer: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {bearer}"))
            .header("content-type", "application/json")
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn get_session_cross_owner_returns_404() {
        let state = state_with_store().await;
        let app = build_router_with_auth(state, two_clients());

        // alice creates a session.
        let create_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("authorization", "Bearer token-a")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create_resp.status(), StatusCode::CREATED);
        let bytes = axum::body::to_bytes(create_resp.into_body(), 4096)
            .await
            .unwrap();
        let info: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let session_id = info["id"].as_str().unwrap().to_owned();

        // alice can read her own session.
        let own_resp = app
            .clone()
            .oneshot(authed("GET", format!("/sessions/{session_id}"), "token-a"))
            .await
            .unwrap();
        assert_eq!(own_resp.status(), StatusCode::OK);

        // bob cannot — indistinguishable from a missing session.
        let foreign_resp = app
            .oneshot(authed("GET", format!("/sessions/{session_id}"), "token-b"))
            .await
            .unwrap();
        assert_eq!(foreign_resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn patch_session_cross_owner_returns_404_and_does_not_update() {
        let state = state_with_store().await;
        let app = build_router_with_auth(state, two_clients());

        let create_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("authorization", "Bearer token-a")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(create_resp.into_body(), 4096)
            .await
            .unwrap();
        let info: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let session_id = info["id"].as_str().unwrap().to_owned();

        let body = serde_json::json!({ "title": "hijacked" });
        let foreign_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/sessions/{session_id}"))
                    .header("authorization", "Bearer token-b")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(foreign_resp.status(), StatusCode::NOT_FOUND);

        // Confirm the title was NOT changed — read it back as the rightful owner.
        let own_resp = app
            .oneshot(authed("GET", format!("/sessions/{session_id}"), "token-a"))
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(own_resp.into_body(), 4096)
            .await
            .unwrap();
        let info: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_ne!(info["title"], "hijacked");
    }

    #[tokio::test]
    async fn delete_session_cross_owner_returns_404_and_does_not_delete() {
        let state = state_with_store().await;
        let app = build_router_with_auth(state, two_clients());

        let create_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("authorization", "Bearer token-a")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(create_resp.into_body(), 4096)
            .await
            .unwrap();
        let info: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let session_id = info["id"].as_str().unwrap().to_owned();

        let foreign_resp = app
            .clone()
            .oneshot(authed(
                "DELETE",
                format!("/sessions/{session_id}"),
                "token-b",
            ))
            .await
            .unwrap();
        assert_eq!(foreign_resp.status(), StatusCode::NOT_FOUND);

        // Still there for the rightful owner.
        let own_resp = app
            .oneshot(authed("GET", format!("/sessions/{session_id}"), "token-a"))
            .await
            .unwrap();
        assert_eq!(own_resp.status(), StatusCode::OK);
    }
}
