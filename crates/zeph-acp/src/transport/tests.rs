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
            auth_clients: Vec::new(),
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

fn test_state_with_session_data_dir(data_dir: std::path::PathBuf) -> AcpHttpState {
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
            auth_clients: Vec::new(),
            discovery_enabled: true,
            terminal_timeout_secs: 120,
            project_rules: vec![],
            title_max_chars: 60,
            max_history: 100,
            sqlite_path: None,
            session_data_dir: Some(data_dir),
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
            auth_clients: Vec::new(),
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

/// agent-client-protocol 2.0.0 added standard-transport support for JSON-RPC batches (an array
/// body instead of a single object). `post_handler` relays the raw HTTP body byte-for-byte into
/// the connection's duplex writer (`transport/http.rs`), so a batch now reaches the SDK's
/// dispatch loop end-to-end without any Zeph-side code change. This test proves both batch
/// entries are actually dispatched and individually answered — not silently dropped or
/// short-circuited after the first entry — by checking the response carries both request ids
/// (see spec.md "Breaking Changes Resolution (SDK 1.2.0 -> 2.0.0)"). Both entries below
/// deliberately omit `params` so both come back as individual `Invalid params` errors rather
/// than a real handshake — dispatch/response-tracking is what's under test, not `initialize`
/// semantics, and the SDK aggregates all batch replies into a single JSON array on one SSE line
/// (confirmed empirically), not one line per entry.
#[tokio::test]
async fn post_batch_body_dispatches_all_entries_and_returns_all_responses() {
    use std::collections::HashSet;
    use std::time::Duration;

    use futures::StreamExt as _;

    // Keep `state` alive for the whole test: it owns the `connections` map, which in turn owns
    // the duplex-pipe writer half and the broadcast sender the SSE stream reads from. Passing
    // only `acp_router(test_state())` inline would drop `state` (and close the pipe) as soon as
    // `oneshot()` returns, before the SSE body below is ever polled.
    let state = test_state();
    let router = acp_router(state.clone());

    let batch = r#"[{"jsonrpc":"2.0","id":1,"method":"initialize"},{"jsonrpc":"2.0","id":2,"method":"initialize"}]"#;
    let req = Request::builder()
        .method("POST")
        .uri("/acp")
        .header("content-type", "application/json")
        .body(Body::from(batch))
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let mut stream = response.into_body().into_data_stream();
    let mut buf = String::new();
    let mut received_ids: HashSet<u64> = HashSet::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);

    while received_ids.len() < 2 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            remaining > Duration::ZERO,
            "timed out waiting for 2 batch responses, got {received_ids:?} so far"
        );
        let chunk = tokio::time::timeout(remaining, stream.next())
            .await
            .expect("timed out waiting for next SSE chunk")
            .expect("SSE stream ended before both batch responses arrived")
            .expect("SSE stream error");
        buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(pos) = buf.find('\n') {
            let line = buf[..pos].to_owned();
            buf.drain(..=pos);
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let Ok(json) = serde_json::from_str::<serde_json::Value>(data.trim()) else {
                continue;
            };
            // Batch replies arrive aggregated as a single JSON array; a non-batch single-object
            // reply (defensive fallback, in case framing ever changes) is handled too.
            let entries: Vec<&serde_json::Value> = match &json {
                serde_json::Value::Array(entries) => entries.iter().collect(),
                other => vec![other],
            };
            for entry in entries {
                if let Some(id) = entry.get("id").and_then(serde_json::Value::as_u64) {
                    received_ids.insert(id);
                }
            }
        }
    }

    assert_eq!(received_ids, HashSet::from([1, 2]));
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
            auth_clients: vec![crate::transport::AcpClientToken {
                id: "default".into(),
                token: (token).into(),
            }],
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
    // test_state() has auth_clients: Vec::new() — no auth layer applied.
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
            auth_clients: vec![crate::transport::AcpClientToken {
                id: "default".into(),
                token: ("secret").into(),
            }],
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
            auth_clients: Vec::new(),
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
            auth_clients: Vec::new(),
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
            auth_clients: Vec::new(),
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
            auth_clients: Vec::new(),
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
    store
        .create_acp_session("sess-1", Some(crate::transport::OWNER_KEY_LOCAL))
        .await
        .unwrap();
    // Regression guard for the S1 false-green (spec-068 §12.3 / D-2): `list_acp_sessions`
    // now derives `message_count` from `acp_sessions.event_count`, so the fixture must drive
    // it through `zeph_session::SessionStore::update_seq` — the same primitive
    // `SessionSink::record_message` calls in production — not the retired `save_acp_event`,
    // which only ever populated the now-permanently-empty `acp_session_events` table.
    let session_store = zeph_session::SessionStore::new(store.pool().clone());
    session_store.update_seq("sess-1", 0, 1).await.unwrap();
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

// ── Owner scoping tests (#5868) ───────────────────────────────────────────────

fn state_with_two_clients() -> AcpHttpState {
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
            auth_clients: vec![
                crate::transport::AcpClientToken {
                    id: "alice".into(),
                    token: "token-a".into(),
                },
                crate::transport::AcpClientToken {
                    id: "bob".into(),
                    token: "token-b".into(),
                },
            ],
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
async fn list_sessions_isolates_distinct_token_clients() {
    use axum::body::to_bytes;

    let store = zeph_memory::store::SqliteStore::new(":memory:")
        .await
        .expect("SqliteStore::new");
    store
        .create_acp_session("alice-sess", Some("alice"))
        .await
        .unwrap();
    store
        .create_acp_session("bob-sess", Some("bob"))
        .await
        .unwrap();

    let state = state_with_two_clients().with_store(store);
    let router = acp_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/sessions")
        .header("authorization", "Bearer token-a")
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "alice-sess");

    let req2 = Request::builder()
        .method("GET")
        .uri("/sessions")
        .header("authorization", "Bearer token-b")
        .body(Body::empty())
        .unwrap();
    let response2 = router.oneshot(req2).await.unwrap();
    assert_eq!(response2.status(), StatusCode::OK);
    let body2 = to_bytes(response2.into_body(), 65536).await.unwrap();
    let json2: serde_json::Value = serde_json::from_slice(&body2).unwrap();
    let arr2 = json2.as_array().unwrap();
    assert_eq!(arr2.len(), 1);
    assert_eq!(arr2[0]["id"], "bob-sess");
}

#[tokio::test]
async fn session_messages_cross_owner_returns_404() {
    let store = zeph_memory::store::SqliteStore::new(":memory:")
        .await
        .expect("SqliteStore::new");
    let session_id = "00000000-0000-0000-0000-000000000002";
    store
        .create_acp_session(session_id, Some("alice"))
        .await
        .unwrap();

    let state = state_with_two_clients().with_store(store);
    let router = acp_router(state);

    let req = Request::builder()
        .method("GET")
        .uri(format!("/sessions/{session_id}/messages"))
        .header("authorization", "Bearer token-b")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
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
    store
        .create_acp_session(session_id, Some(crate::transport::OWNER_KEY_LOCAL))
        .await
        .unwrap();

    // Regression guard for the S1-class false-green: message_messages_handler now reads the
    // durable JSONL event log (spec-068 §12.3 / D-2), not the legacy acp_session_events table —
    // seed the log directly instead of the retired save_acp_event write path.
    let dir = tempfile::tempdir().unwrap();
    let session_path = zeph_session::session_dir(dir.path(), session_id);
    let log = zeph_session::SessionEventLog::open(&session_path)
        .await
        .unwrap();
    log.append(
        None,
        None,
        zeph_session::SessionEvent::UserMessage {
            text: "hello".to_owned(),
            image_refs: Vec::new(),
        },
    )
    .await
    .unwrap();
    drop(log);

    let state = test_state_with_session_data_dir(dir.path().to_path_buf()).with_store(store);
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
