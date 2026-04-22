// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#![cfg(all(test, feature = "acp-http"))]

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use agent_client_protocol;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt as _;
use zeph_core::channel::LoopbackChannel;

use crate::agent::{AcpContext, SendAgentSpawner, SessionContext};
use crate::transport::http::{AcpHttpState, ConnectionHandle};
use crate::transport::router::acp_router;
use crate::transport::{AcpServerConfig, SharedAvailableModels};

fn shared_models(models: Vec<String>) -> SharedAvailableModels {
    std::sync::Arc::new(parking_lot::RwLock::new(models))
}

fn noop_spawner() -> SendAgentSpawner {
    Arc::new(
        |_channel: LoopbackChannel, _ctx: Option<AcpContext>, _session_ctx: SessionContext| {
            Box::pin(async {}) as Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>
        },
    )
}

fn test_state() -> AcpHttpState {
    AcpHttpState::new(
        noop_spawner(),
        AcpServerConfig {
            agent_name: "test".into(),
            agent_version: "0.0.1".into(),
            max_sessions: 4,
            session_idle_timeout_secs: 1800,
            permission_file: None,
            provider_factory: None,
            available_models: shared_models(vec![]),
            mcp_manager: None,
            auth_bearer_token: None,
            discovery_enabled: true,
            terminal_timeout_secs: 120,
            project_rules: vec![],
            title_max_chars: 60,
            max_history: 100,
            sqlite_path: None,
            ready_notification: None,
            ..Default::default()
        },
    )
    .with_ready(true)
}

fn state_with_max_sessions(max: usize) -> AcpHttpState {
    AcpHttpState::new(
        noop_spawner(),
        AcpServerConfig {
            agent_name: "test".into(),
            agent_version: "0.0.1".into(),
            max_sessions: max,
            session_idle_timeout_secs: 1800,
            permission_file: None,
            provider_factory: None,
            available_models: shared_models(vec![]),
            mcp_manager: None,
            auth_bearer_token: None,
            discovery_enabled: true,
            terminal_timeout_secs: 120,
            project_rules: vec![],
            title_max_chars: 60,
            max_history: 100,
            sqlite_path: None,
            ready_notification: None,
            ..Default::default()
        },
    )
    .with_ready(true)
}

// ── POST /acp tests ──────────────────────────────────────────────────────────

#[tokio::test]
async fn post_without_session_id_creates_new_connection_and_returns_sse() {
    let router = acp_router(test_state());

    let req = Request::builder()
        .method("POST")
        .uri("/acp")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
        ))
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().contains_key("acp-session-id"));
    let ct = response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        ct.contains("text/event-stream"),
        "expected SSE content-type, got: {ct}"
    );
}

#[tokio::test]
async fn post_with_existing_session_id_reuses_connection() {
    let state = test_state();
    let router = acp_router(state.clone());

    // First request — create session
    let req = Request::builder()
        .method("POST")
        .uri("/acp")
        .body(Body::from("{}"))
        .unwrap();
    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let session_id = response
        .headers()
        .get("acp-session-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();

    // Second request — reuse session
    let router2 = acp_router(state);
    let req2 = Request::builder()
        .method("POST")
        .uri("/acp")
        .header("acp-session-id", &session_id)
        .body(Body::from("{}"))
        .unwrap();
    let response2 = router2.oneshot(req2).await.unwrap();
    assert_eq!(response2.status(), StatusCode::OK);
    assert_eq!(
        response2
            .headers()
            .get("acp-session-id")
            .unwrap()
            .to_str()
            .unwrap(),
        session_id
    );
}

#[tokio::test]
async fn post_with_unknown_session_id_returns_not_found() {
    let router = acp_router(test_state());

    let req = Request::builder()
        .method("POST")
        .uri("/acp")
        .header("acp-session-id", "00000000-0000-0000-0000-000000000000")
        .body(Body::from("{}"))
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn post_with_malformed_session_id_returns_bad_request() {
    let router = acp_router(test_state());

    let req = Request::builder()
        .method("POST")
        .uri("/acp")
        .header("acp-session-id", "not-a-uuid!!!")
        .body(Body::from("{}"))
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn post_returns_503_when_max_sessions_reached() {
    let state = state_with_max_sessions(0);
    let router = acp_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/acp")
        .body(Body::from("{}"))
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn post_returns_500_when_writer_is_closed() {
    use tokio::sync::Mutex;
    use tokio::sync::broadcast;

    let state = test_state();

    // Inject a broken (closed) DuplexStream writer by creating a pair and
    // immediately dropping the reader half so writes will fail.
    let (_, dead_writer) = tokio::io::duplex(64);
    let (tx, _) = broadcast::channel::<String>(4);
    let session_id = uuid::Uuid::new_v4().to_string();
    let handle = Arc::new(ConnectionHandle {
        writer: Arc::new(Mutex::new(dead_writer)),
        output_tx: tx,
        last_activity: AtomicU64::new(0),
        idle_timeout_secs: 1800,
    });
    state.connections.insert(session_id.clone(), handle);

    let router = acp_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/acp")
        .header("acp-session-id", &session_id)
        .body(Body::from("{}"))
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

// ── GET /acp tests ───────────────────────────────────────────────────────────

#[tokio::test]
async fn get_without_session_id_returns_bad_request() {
    let router = acp_router(test_state());

    let req = Request::builder()
        .method("GET")
        .uri("/acp")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_with_unknown_session_id_returns_not_found() {
    let router = acp_router(test_state());

    let req = Request::builder()
        .method("GET")
        .uri("/acp")
        .header("acp-session-id", "00000000-0000-0000-0000-000000000000")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_with_malformed_session_id_returns_bad_request() {
    let router = acp_router(test_state());

    // "not-a-uuid" is a valid header value but fails UUID parsing.
    let req = Request::builder()
        .method("GET")
        .uri("/acp")
        .header("acp-session-id", "not-a-uuid-string")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ── GET /acp/ws tests ────────────────────────────────────────────────────────

/// Bind a real TCP listener, serve the router on it, and return the bound address.
async fn serve_on_random_port(router: axum::Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

#[tokio::test]
async fn ws_upgrade_returns_101_switching_protocols() {
    use tokio_tungstenite::connect_async;

    let router = acp_router(test_state());
    let addr = serve_on_random_port(router).await;

    let url = format!("ws://{addr}/acp/ws");
    let result = connect_async(&url).await;
    assert!(
        result.is_ok(),
        "WebSocket connect should succeed: {result:?}"
    );
}

#[tokio::test]
async fn ws_upgrade_returns_503_when_max_sessions_reached() {
    use tokio_tungstenite::connect_async;

    let router = acp_router(state_with_max_sessions(0));
    let addr = serve_on_random_port(router).await;

    let url = format!("ws://{addr}/acp/ws");
    let result = connect_async(&url).await;
    // Server returns 503, tungstenite yields a non-101 HTTP error.
    assert!(result.is_err(), "connect should fail with 503");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("503") || err.contains("Service Unavailable"),
        "expected 503 in error, got: {err}"
    );
}

// ── Bearer auth tests ─────────────────────────────────────────────────────────

fn state_with_auth(token: &str) -> AcpHttpState {
    AcpHttpState::new(
        noop_spawner(),
        AcpServerConfig {
            agent_name: "test".into(),
            agent_version: "0.0.1".into(),
            max_sessions: 4,
            session_idle_timeout_secs: 1800,
            permission_file: None,
            provider_factory: None,
            available_models: shared_models(vec![]),
            mcp_manager: None,
            auth_bearer_token: Some(token.into()),
            discovery_enabled: true,
            terminal_timeout_secs: 120,
            project_rules: vec![],
            title_max_chars: 60,
            max_history: 100,
            sqlite_path: None,
            ready_notification: None,
            ..Default::default()
        },
    )
    .with_ready(true)
}

#[tokio::test]
async fn auth_valid_token_passes() {
    let router = acp_router(state_with_auth("secret"));

    let req = Request::builder()
        .method("POST")
        .uri("/acp")
        .header("content-type", "application/json")
        .header("authorization", "Bearer secret")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
        ))
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn auth_missing_token_returns_401() {
    let router = acp_router(state_with_auth("secret"));

    let req = Request::builder()
        .method("POST")
        .uri("/acp")
        .body(Body::from("{}"))
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_wrong_token_returns_401() {
    let router = acp_router(state_with_auth("secret"));

    let req = Request::builder()
        .method("POST")
        .uri("/acp")
        .header("authorization", "Bearer wrong")
        .body(Body::from("{}"))
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_none_mode_allows_all_requests() {
    // test_state() has auth_bearer_token: None — no auth layer applied.
    let router = acp_router(test_state());

    let req = Request::builder()
        .method("POST")
        .uri("/acp")
        .body(Body::from("{}"))
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    // Any non-401 status confirms auth is not enforced.
    assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn health_is_public_even_when_bearer_auth_is_enabled() {
    let router = acp_router(state_with_auth("secret"));

    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn health_returns_200_when_ready() {
    use axum::body::to_bytes;

    let router = acp_router(test_state());
    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "ok");
    assert_eq!(json["version"], "0.0.1");
    assert!(json["uptime_secs"].is_u64());
}

#[tokio::test]
async fn health_returns_503_when_not_ready() {
    use axum::body::to_bytes;

    let state = AcpHttpState::new(
        noop_spawner(),
        AcpServerConfig {
            agent_name: "test".into(),
            agent_version: "0.0.1".into(),
            max_sessions: 4,
            session_idle_timeout_secs: 1800,
            permission_file: None,
            provider_factory: None,
            available_models: std::sync::Arc::new(parking_lot::RwLock::new(Vec::new())),
            mcp_manager: None,
            auth_bearer_token: Some("secret".into()),
            discovery_enabled: true,
            terminal_timeout_secs: 120,
            project_rules: vec![],
            title_max_chars: 60,
            max_history: 100,
            sqlite_path: None,
            ready_notification: None,
            ..Default::default()
        },
    );
    let router = acp_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "starting");
}

#[tokio::test]
async fn acp_post_returns_503_when_server_not_ready() {
    let state = AcpHttpState::new(
        noop_spawner(),
        AcpServerConfig {
            agent_name: "test".into(),
            agent_version: "0.0.1".into(),
            max_sessions: 4,
            session_idle_timeout_secs: 1800,
            permission_file: None,
            provider_factory: None,
            available_models: std::sync::Arc::new(parking_lot::RwLock::new(Vec::new())),
            mcp_manager: None,
            auth_bearer_token: None,
            discovery_enabled: true,
            terminal_timeout_secs: 120,
            project_rules: vec![],
            title_max_chars: 60,
            max_history: 100,
            sqlite_path: None,
            ready_notification: None,
            ..Default::default()
        },
    );
    let router = acp_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/acp")
        .body(Body::from("{}"))
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

// ── Discovery endpoint tests ──────────────────────────────────────────────────

#[tokio::test]
async fn discovery_returns_expected_json_fields() {
    use axum::body::to_bytes;

    let router = acp_router(test_state());

    let req = Request::builder()
        .method("GET")
        .uri("/.well-known/acp.json")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), 1_048_576).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["name"], "test");
    assert_eq!(json["version"], "0.0.1");
    assert!(
        json["transports"].is_object(),
        "transports must be an object"
    );
    assert!(json["transports"]["http_sse"].is_object());
    assert!(json["transports"]["websocket"].is_object());
    assert!(json["transports"]["health"].is_object());
    assert!(
        json["authentication"].is_null(),
        "authentication must be null when no token"
    );
    assert_eq!(json["readiness"]["stdio_notification"], "zeph/ready");
    assert_eq!(json["readiness"]["http_health_endpoint"], "/health");
    // protocol_version must be the integer value of ProtocolVersion::LATEST (1).
    assert_eq!(
        json["protocol_version"],
        serde_json::json!(agent_client_protocol::schema::ProtocolVersion::LATEST)
    );
}

#[tokio::test]
async fn discovery_with_bearer_token_returns_bearer_auth_type() {
    use axum::body::to_bytes;

    let router = acp_router(state_with_auth("secret"));

    let req = Request::builder()
        .method("GET")
        .uri("/.well-known/acp.json")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), 1_048_576).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["authentication"]["type"], "bearer");
}

#[tokio::test]
async fn discovery_disabled_returns_404() {
    let state = AcpHttpState::new(
        noop_spawner(),
        AcpServerConfig {
            agent_name: "test".into(),
            agent_version: "0.0.1".into(),
            max_sessions: 4,
            session_idle_timeout_secs: 1800,
            permission_file: None,
            provider_factory: None,
            available_models: shared_models(vec![]),
            mcp_manager: None,
            auth_bearer_token: None,
            discovery_enabled: false,
            terminal_timeout_secs: 120,
            project_rules: vec![],
            title_max_chars: 60,
            max_history: 100,
            sqlite_path: None,
            ready_notification: None,
            ..Default::default()
        },
    );
    let router = acp_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/.well-known/acp.json")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ── agent.json endpoint tests ─────────────────────────────────────────────────

#[tokio::test]
async fn agent_json_returns_expected_fields() {
    use axum::body::to_bytes;

    let router = acp_router(test_state());

    let req = Request::builder()
        .method("GET")
        .uri("/agent.json")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), 1_048_576).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["id"], "zeph");
    assert_eq!(json["name"], "test");
    assert_eq!(json["version"], "0.0.1");
    assert!(
        json["description"].is_string(),
        "description must be a string"
    );
    assert!(
        json["distribution"].is_object(),
        "distribution must be an object"
    );
    assert_eq!(json["distribution"]["type"], "binary");
    assert!(
        json["distribution"]["platforms"].is_array(),
        "platforms must be an array"
    );
}

#[tokio::test]
async fn agent_json_disabled_returns_404() {
    let state = AcpHttpState::new(
        noop_spawner(),
        AcpServerConfig {
            agent_name: "test".into(),
            agent_version: "0.0.1".into(),
            max_sessions: 4,
            session_idle_timeout_secs: 1800,
            permission_file: None,
            provider_factory: None,
            available_models: shared_models(vec![]),
            mcp_manager: None,
            auth_bearer_token: None,
            discovery_enabled: false,
            terminal_timeout_secs: 120,
            project_rules: vec![],
            title_max_chars: 60,
            max_history: 100,
            sqlite_path: None,
            ready_notification: None,
            ..Default::default()
        },
    );
    let router = acp_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/agent.json")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ── Reaper test ───────────────────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn reaper_removes_expired_connections() {
    use std::time::Duration;
    use tokio::sync::Mutex;
    use tokio::sync::broadcast;

    let state = AcpHttpState::new(
        noop_spawner(),
        AcpServerConfig {
            agent_name: "test".into(),
            agent_version: "0.0.1".into(),
            max_sessions: 4,
            session_idle_timeout_secs: 30,
            permission_file: None,
            provider_factory: None,
            available_models: shared_models(vec![]),
            mcp_manager: None,
            auth_bearer_token: None,
            discovery_enabled: true,
            terminal_timeout_secs: 120,
            project_rules: vec![],
            title_max_chars: 60,
            max_history: 100,
            sqlite_path: None,
            ready_notification: None,
            ..Default::default()
        },
    );

    // Insert a connection with last_activity in the far past (expired).
    let (_, writer) = tokio::io::duplex(64);
    let (tx, _) = broadcast::channel::<String>(4);
    let expired_id = uuid::Uuid::new_v4().to_string();
    state.connections.insert(
        expired_id.clone(),
        Arc::new(ConnectionHandle {
            writer: Arc::new(Mutex::new(writer)),
            output_tx: tx,
            // Set last_activity to 0 (Unix epoch) so it's always expired.
            last_activity: AtomicU64::new(0),
            idle_timeout_secs: 30,
        }),
    );

    assert_eq!(state.connections.len(), 1);
    state.start_reaper();

    // Advance time past the reaper interval (60 s).
    tokio::time::advance(Duration::from_secs(61)).await;
    // Yield to let the reaper task run.
    tokio::task::yield_now().await;

    assert_eq!(
        state.connections.len(),
        0,
        "reaper should have removed the expired connection"
    );
}

// ── GET /sessions tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn list_sessions_returns_503_when_store_is_none() {
    let router = acp_router(test_state());

    let req = Request::builder()
        .method("GET")
        .uri("/sessions")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn list_sessions_returns_empty_array_when_no_sessions() {
    use axum::body::to_bytes;

    let store = zeph_memory::store::SqliteStore::new(":memory:")
        .await
        .expect("SqliteStore::new");
    let state = test_state().with_store(store);
    let router = acp_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/sessions")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json, serde_json::json!([]));
}

#[tokio::test]
async fn list_sessions_returns_session_data() {
    use axum::body::to_bytes;

    let store = zeph_memory::store::SqliteStore::new(":memory:")
        .await
        .expect("SqliteStore::new");
    store.create_acp_session("sess-1").await.unwrap();
    store
        .save_acp_event("sess-1", "user", "hello")
        .await
        .unwrap();
    store
        .update_session_title("sess-1", "Test Session")
        .await
        .unwrap();

    let state = test_state().with_store(store);
    let router = acp_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/sessions")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "sess-1");
    assert_eq!(arr[0]["title"], "Test Session");
    assert_eq!(arr[0]["message_count"], 1);
}

// ── GET /sessions/{id}/messages tests ────────────────────────────────────────

#[tokio::test]
async fn session_messages_returns_503_when_store_is_none() {
    let router = acp_router(test_state());

    let req = Request::builder()
        .method("GET")
        .uri("/sessions/00000000-0000-0000-0000-000000000001/messages")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn session_messages_returns_400_for_non_uuid() {
    let store = zeph_memory::store::SqliteStore::new(":memory:")
        .await
        .expect("SqliteStore::new");
    let state = test_state().with_store(store);
    let router = acp_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/sessions/not-a-uuid/messages")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn session_messages_returns_404_for_unknown_session() {
    let store = zeph_memory::store::SqliteStore::new(":memory:")
        .await
        .expect("SqliteStore::new");
    let state = test_state().with_store(store);
    let router = acp_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/sessions/00000000-0000-0000-0000-000000000099/messages")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn session_messages_returns_events_for_known_session() {
    use axum::body::to_bytes;

    let store = zeph_memory::store::SqliteStore::new(":memory:")
        .await
        .expect("SqliteStore::new");
    let session_id = "00000000-0000-0000-0000-000000000001";
    store.create_acp_session(session_id).await.unwrap();
    store
        .save_acp_event(session_id, "user_message", "hello")
        .await
        .unwrap();

    let state = test_state().with_store(store);
    let router = acp_router(state);

    let req = Request::builder()
        .method("GET")
        .uri(format!("/sessions/{session_id}/messages"))
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["event_type"], "user_message");
    assert_eq!(arr[0]["payload"], "hello");
    assert!(arr[0]["created_at"].is_string());
}
