// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#![cfg(all(test, feature = "acp-http"))]

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt as _;
use zeph_core::channel::LoopbackChannel;

use crate::agent::{AcpContext, SendAgentSpawner};
use crate::transport::AcpServerConfig;
use crate::transport::http::{AcpHttpState, ConnectionHandle};
use crate::transport::router::acp_router;

fn noop_spawner() -> SendAgentSpawner {
    Arc::new(|_channel: LoopbackChannel, _ctx: Option<AcpContext>| {
        Box::pin(async {}) as Pin<Box<dyn std::future::Future<Output = ()> + 'static>>
    })
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
            available_models: vec![],
            mcp_manager: None,
        },
    )
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
            available_models: vec![],
            mcp_manager: None,
        },
    )
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
            available_models: vec![],
            mcp_manager: None,
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
