// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Axum router construction with auth middleware, rate limiting, body size limits, and
//! IBCT (Invocation-Bound Capability Token) verification.
//!
//! [`build_router_with_full_config`] is the production entry point called by [`A2aServer::serve`].
//! The test-only [`build_router_with_config`] omits `require_auth`, `max_body_size`, and IBCT
//! enforcement for convenience.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tower_http::limit::RequestBodyLimitLayer;
use zeph_common::http_middleware::{
    AuthConfig, RateLimitState, auth_middleware, rate_limit_middleware,
};

use super::handlers::{agent_card_handler, jsonrpc_handler, stream_handler};
use super::state::AppState;
use crate::ibct::{Ibct, IbctKey};

#[cfg(test)]
const DEFAULT_MAX_BODY_SIZE: usize = 1024 * 1024; // 1 MiB

/// Fallback upper bound on the request body buffered by [`ibct_middleware`] to peek at
/// `task_id`, used only when [`IbctConfig`] was not given an explicit `max_body_size`.
///
/// [`tower_http::limit::RequestBodyLimitLayer`] (the outermost layer — see
/// [`build_router_with_full_config`]) already rejects oversized requests with `413` before
/// this middleware runs, so this is a secondary bound, not the real enforcement point. In
/// production [`IbctConfig::new`] is always given the server's real `max_body_size`, so a
/// legitimate request between this constant and a larger configured `max_body_size` cannot
/// pass the outer layer and then be spuriously rejected here.
const DEFAULT_IBCT_BODY_PEEK: usize = 8 * 1024 * 1024;

/// Server-side state for [`ibct_middleware`]: the verification key set and the `endpoint`
/// every verified token must be scoped to.
///
/// Constructed by [`A2aServer::with_ibct_keys`](crate::server::A2aServer::with_ibct_keys) /
/// [`A2aServer::serve`](crate::server::A2aServer::serve) from the server's own advertised
/// `AgentCard::url`. An empty key set (the [`Default`]) disables IBCT enforcement entirely —
/// mirrors the existing bearer-auth opt-in pattern ([`AuthConfig`]).
#[derive(Clone)]
pub struct IbctConfig {
    keys: Arc<Vec<IbctKey>>,
    endpoint: Arc<str>,
    max_body_size: usize,
}

impl IbctConfig {
    /// Construct an [`IbctConfig`]. Pass an empty `keys` to disable enforcement.
    ///
    /// `max_body_size` should match the server's real
    /// [`with_max_body_size`](crate::server::A2aServer::with_max_body_size) so the IBCT
    /// body-peek bound never falls below what the outer `RequestBodyLimitLayer` already
    /// allows through.
    #[must_use]
    pub fn new(keys: Vec<IbctKey>, endpoint: impl Into<Arc<str>>, max_body_size: usize) -> Self {
        Self {
            keys: Arc::new(keys),
            endpoint: endpoint.into(),
            max_body_size,
        }
    }

    fn is_enforced(&self) -> bool {
        !self.keys.is_empty()
    }
}

impl Default for IbctConfig {
    fn default() -> Self {
        Self::new(Vec::new(), "", DEFAULT_IBCT_BODY_PEEK)
    }
}

/// Axum middleware that enforces IBCT (Invocation-Bound Capability Token) verification.
///
/// No-op when [`IbctConfig`] carries no keys (`ibct_keys` unset in server config — matches
/// the existing bearer-auth opt-in pattern). When keys are configured:
///
/// - Missing or undecodable `X-Zeph-IBCT` header → `401 Unauthorized`.
/// - Header present but fails [`Ibct::verify`] (bad signature, expired, unknown key, or
///   endpoint/task mismatch) → `403 Forbidden`.
/// - Header present and valid → passes through.
///
/// The expected `task_id` is read from the JSON-RPC request body: `params.id` for
/// `tasks/get`/`tasks/cancel`, `params.message.taskId` for `message/send`/`message/stream`.
/// A brand-new task (no `taskId` yet — the server has not assigned one) is checked against
/// the empty-string sentinel, so a client scopes its very first IBCT for a task with
/// `Ibct::issue("", endpoint, ttl, key)`.
///
/// **Known scope limitation (MVP, #6260 review S3)**: `handle_send_message` always creates a
/// fresh task (`TaskManager::create_task` mints a new UUID unconditionally — it never resumes
/// an existing one from `message.taskId`), so every `message/send` request is scoped to the
/// same empty-string `task_id`, not to that specific invocation. The "invocation-bound"
/// per-task property therefore only holds for `tasks/get`/`tasks/cancel`, which *do* validate
/// against a caller-supplied, task-specific ID — a token cannot be forged for a victim's
/// existing task. A captured, still-valid `""`-scoped token is replayable to start arbitrary
/// *new* tasks against the same endpoint until it expires (bounded by `ibct_ttl_secs`, and
/// already true regardless of this limitation since `Ibct::verify` has no single-use/nonce
/// dedup — see the `ibct` module docs). Binding new-task tokens to the client-supplied
/// `message.messageId` instead of the empty sentinel would close this gap; deferred rather
/// than done here to keep this fix scoped to wiring the existing primitive.
///
/// Must be layered *inside* `auth_middleware` (i.e. auth runs first): IBCT is a second,
/// finer-grained authorization check layered on top of the coarse bearer gate, not a
/// replacement for it.
///
/// # Errors
///
/// Returns `401 Unauthorized` or `403 Forbidden` per the rules above.
#[tracing::instrument(skip_all, name = "a2a.server.ibct")]
pub async fn ibct_middleware(
    axum::extract::State(cfg): axum::extract::State<IbctConfig>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if !cfg.is_enforced() {
        return next.run(req).await;
    }

    let header = req
        .headers()
        .get("x-zeph-ibct")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let Some(header) = header else {
        tracing::warn!("a2a ibct: missing X-Zeph-IBCT header while ibct_keys is configured");
        return StatusCode::UNAUTHORIZED.into_response();
    };

    let token = match Ibct::decode(&header) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("a2a ibct: failed to decode X-Zeph-IBCT header: {e}");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };

    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, cfg.max_body_size).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("a2a ibct: failed to buffer request body: {e}");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    let task_id = extract_task_id(&bytes);

    if let Err(e) = token.verify(&cfg.keys, &cfg.endpoint, &task_id) {
        tracing::warn!("a2a ibct: verification failed: {e}");
        return StatusCode::FORBIDDEN.into_response();
    }

    let req = Request::from_parts(parts, Body::from(bytes));
    next.run(req).await
}

/// Extracts the expected `task_id` for IBCT scoping from a raw `/a2a` or `/a2a/stream`
/// request body, dispatching on the JSON-RPC `method` field so a method that happens to reuse
/// a field name from a different method's schema can never be misread: `params.id` for
/// `tasks/get`/`tasks/cancel`, `params.message.taskId` for every other (or absent — `/a2a/stream`
/// carries no top-level `method`) method. Returns `""` when the expected field is absent — an
/// unparseable body, or a brand-new task that has no `taskId` yet (see [`ibct_middleware`]'s
/// doc comment for the empty-string sentinel this implies for `message/send`/`message/stream`).
fn extract_task_id(bytes: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return String::new();
    };
    let Some(params) = value.get("params") else {
        return String::new();
    };
    let is_task_id_method = matches!(
        value.get("method").and_then(serde_json::Value::as_str),
        Some("tasks/get" | "tasks/cancel")
    );
    if is_task_id_method {
        return params
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .unwrap_or_default();
    }
    params
        .get("message")
        .and_then(|m| m.get("taskId"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_default()
}

#[cfg(test)]
pub fn build_router_with_config(
    state: AppState,
    auth_token: Option<&str>,
    rate_limit: u32,
) -> Router {
    build_router_with_full_config(
        state,
        AuthConfig::new(auth_token, false),
        rate_limit,
        DEFAULT_MAX_BODY_SIZE,
        IbctConfig::default(),
    )
}

pub fn build_router_with_full_config(
    state: AppState,
    auth_cfg: AuthConfig,
    rate_limit: u32,
    max_body_size: usize,
    ibct_cfg: IbctConfig,
) -> Router {
    let rate_state = RateLimitState::new(rate_limit, &[]);

    let protected = Router::new()
        .route("/a2a", post(jsonrpc_handler))
        .route("/a2a/stream", post(stream_handler))
        .layer(middleware::from_fn_with_state(ibct_cfg, ibct_middleware))
        .layer(middleware::from_fn_with_state(auth_cfg, auth_middleware))
        .layer(middleware::from_fn_with_state(
            rate_state,
            rate_limit_middleware,
        ))
        .layer(RequestBodyLimitLayer::new(max_body_size));

    Router::new()
        .route("/.well-known/agent.json", get(agent_card_handler))
        .merge(protected)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::IpAddr;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use axum::body::Body;
    use tokio::sync::Mutex;
    use tower::ServiceExt;
    use zeph_common::http_middleware::{MAX_RATE_LIMIT_ENTRIES, RATE_WINDOW};

    use super::*;
    use crate::server::testing::test_state;

    #[tokio::test]
    async fn auth_allows_valid_token() {
        let app = build_router_with_config(test_state(), Some("secret-token"), 0);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "tasks/get",
            "params": {"id": "x"}
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret-token")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn auth_rejects_missing_token() {
        let app = build_router_with_config(test_state(), Some("secret-token"), 0);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "tasks/get",
            "params": {"id": "x"}
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn auth_rejects_wrong_token() {
        let app = build_router_with_config(test_state(), Some("secret-token"), 0);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "tasks/get",
            "params": {"id": "x"}
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .header("authorization", "Bearer wrong-token")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn agent_card_skips_auth() {
        let app = build_router_with_config(test_state(), Some("secret-token"), 0);

        let req = axum::http::Request::builder()
            .uri("/.well-known/agent.json")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn no_auth_when_token_unset() {
        let app = build_router_with_config(test_state(), None, 0);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "tasks/get",
            "params": {"id": "x"}
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn body_size_limit() {
        let app = build_router_with_config(test_state(), None, 0);

        let oversized = vec![b'a'; DEFAULT_MAX_BODY_SIZE + 1];
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(oversized))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 413);
    }

    #[tokio::test]
    async fn auth_rejects_bearer_prefix_only() {
        let app = build_router_with_config(test_state(), Some("secret"), 0);

        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": "1",
            "method": "tasks/get", "params": {"id": "x"}
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .header("authorization", "Bearer ")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn auth_rejects_non_bearer_scheme() {
        let app = build_router_with_config(test_state(), Some("secret"), 0);

        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": "1",
            "method": "tasks/get", "params": {"id": "x"}
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .header("authorization", "Basic c2VjcmV0")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn rate_limit_rejects_after_exceeding() {
        use tower::Service;

        let state = test_state();
        let mut app = build_router_with_config(state, None, 2);

        let make_req = || {
            let body = serde_json::json!({
                "jsonrpc": "2.0", "id": "1",
                "method": "tasks/get", "params": {"id": "x"}
            });
            axum::http::Request::builder()
                .method("POST")
                .uri("/a2a")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap()
        };

        // First two requests should succeed (limit=2)
        let resp = app.call(make_req()).await.unwrap();
        assert_eq!(resp.status(), 200, "request 1 should pass");
        let resp = app.call(make_req()).await.unwrap();
        assert_eq!(resp.status(), 200, "request 2 should pass");

        // Third request should be rate-limited
        let resp = app.call(make_req()).await.unwrap();
        assert_eq!(resp.status(), 429, "request 3 should be rate-limited");
    }

    #[tokio::test]
    async fn failed_auth_requests_are_rate_limited() {
        use tower::Service;

        // Regression test for #6110: repeated failed-auth requests from the same IP must
        // still increment the rate-limit counter, so a bearer-token brute-force gets
        // throttled with 429 instead of bypassing rate limiting via 401 short-circuits.
        let mut app = build_router_with_config(test_state(), Some("secret-token"), 2);

        let make_req = || {
            let body = serde_json::json!({
                "jsonrpc": "2.0", "id": "1",
                "method": "tasks/get", "params": {"id": "x"}
            });
            axum::http::Request::builder()
                .method("POST")
                .uri("/a2a")
                .header("content-type", "application/json")
                .header("authorization", "Bearer wrong-token")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap()
        };

        let resp = app.call(make_req()).await.unwrap();
        assert_eq!(resp.status(), 401);
        let resp = app.call(make_req()).await.unwrap();
        assert_eq!(resp.status(), 401);
        let resp = app.call(make_req()).await.unwrap();
        assert_eq!(
            resp.status(),
            429,
            "third failed-auth request from the same IP must be rate-limited"
        );
    }

    fn ip_from_index(i: usize) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::new(
            u8::try_from((i >> 16) & 0xFF).unwrap(),
            u8::try_from((i >> 8) & 0xFF).unwrap(),
            u8::try_from(i & 0xFF).unwrap(),
            1,
        ))
    }

    #[tokio::test]
    async fn max_entries_cap_rejects_when_all_entries_fresh() {
        // Fill map with fresh entries (within RATE_WINDOW) so retain() keeps them all.
        // After retain() the map is still at capacity, so the middleware returns 429.
        let counters = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut map = counters.lock().await;
            let fresh = Instant::now();
            for i in 0..MAX_RATE_LIMIT_ENTRIES {
                let ip = ip_from_index(i);
                map.insert(ip, (1, fresh));
            }
            assert_eq!(map.len(), MAX_RATE_LIMIT_ENTRIES);
        }

        let new_ip = IpAddr::V4(std::net::Ipv4Addr::BROADCAST);

        // Simulate middleware logic: cap exceeded, run retain(), still full → 429
        let now = Instant::now();
        let mut map = counters.lock().await;
        let before = map.len();
        map.retain(|_, (_, ts)| now.duration_since(*ts) < RATE_WINDOW);
        let after = map.len();

        // All entries are fresh so retain() must not remove any
        assert_eq!(after, before, "retain must preserve fresh entries");
        // Map still at capacity: a new IP would be rejected
        assert!(
            after >= MAX_RATE_LIMIT_ENTRIES && !map.contains_key(&new_ip),
            "new IP should be rejected when map is still at capacity after eviction"
        );
    }

    #[tokio::test]
    async fn max_entries_cap_allows_after_stale_eviction() {
        // Fill map with stale entries. After retain() the map is empty, new IP is accepted.
        let counters = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut map = counters.lock().await;
            let stale = Instant::now().checked_sub(Duration::from_mins(2)).unwrap();
            for i in 0..MAX_RATE_LIMIT_ENTRIES {
                let ip = ip_from_index(i);
                map.insert(ip, (1, stale));
            }
        }

        let now = Instant::now();
        let mut map = counters.lock().await;
        map.retain(|_, (_, ts)| now.duration_since(*ts) < RATE_WINDOW);

        // All entries were stale; map should now be empty
        assert_eq!(map.len(), 0, "stale entries must be evicted by retain");
    }

    #[tokio::test]
    async fn eviction_removes_stale_entries() {
        let counters = Arc::new(Mutex::new(HashMap::new()));
        let stale_time = Instant::now().checked_sub(Duration::from_mins(2)).unwrap();
        let fresh_time = Instant::now();

        let stale_ip = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1));
        let fresh_ip = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 2));

        {
            let mut map = counters.lock().await;
            map.insert(stale_ip, (5, stale_time));
            map.insert(fresh_ip, (3, fresh_time));
        }

        // Simulate eviction logic
        let now = Instant::now();
        let mut map = counters.lock().await;
        map.retain(|_, (_, ts)| now.duration_since(*ts) < RATE_WINDOW);

        assert!(
            !map.contains_key(&stale_ip),
            "stale entry should be evicted"
        );
        assert!(map.contains_key(&fresh_ip), "fresh entry should remain");
    }

    #[tokio::test]
    async fn require_auth_rejects_when_no_token_configured() {
        let app = build_router_with_full_config(
            test_state(),
            AuthConfig::new(None, true),
            0,
            DEFAULT_MAX_BODY_SIZE,
            IbctConfig::default(),
        );

        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": "1",
            "method": "tasks/get", "params": {"id": "x"}
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    /// Verify that `build_router_with_full_config` with `rate_limit` > 0 constructs a valid
    /// router using inline GC eviction (shared middleware, no background task required).
    #[tokio::test]
    async fn build_router_with_rate_limit_succeeds() {
        let _router = build_router_with_full_config(
            test_state(),
            AuthConfig::new(None, false),
            5,
            1024 * 1024,
            IbctConfig::default(),
        );
        // Reaching here confirms inline-GC-based router builds without panic.
    }

    #[tokio::test]
    async fn require_auth_false_allows_unauthenticated() {
        let app = build_router_with_full_config(
            test_state(),
            AuthConfig::new(None, false),
            0,
            DEFAULT_MAX_BODY_SIZE,
            IbctConfig::default(),
        );

        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": "1",
            "method": "tasks/get", "params": {"id": "x"}
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
    }
}

#[cfg(all(test, feature = "ibct"))]
mod ibct_middleware_tests {
    use axum::Router;
    use axum::body::Body;
    use std::time::Duration;
    use tower::ServiceExt;

    use super::{AuthConfig, DEFAULT_MAX_BODY_SIZE, IbctConfig, build_router_with_full_config};
    use crate::ibct::{Ibct, IbctKey, ibct_scope_origin};
    use crate::server::testing::test_state;

    const TEST_ENDPOINT: &str = "http://localhost:8080";

    fn test_key() -> IbctKey {
        IbctKey {
            key_id: "k1".into(),
            key_bytes: b"router-test-secret-key".to_vec(),
        }
    }

    fn ibct_app(keys: Vec<IbctKey>) -> Router {
        build_router_with_full_config(
            test_state(),
            AuthConfig::new(None, false),
            0,
            DEFAULT_MAX_BODY_SIZE,
            IbctConfig::new(keys, TEST_ENDPOINT, DEFAULT_MAX_BODY_SIZE),
        )
    }

    fn get_task_request(header: Option<&str>) -> axum::http::Request<Body> {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": "1",
            "method": "tasks/get", "params": {"id": "task-1"}
        });
        let mut builder = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json");
        if let Some(h) = header {
            builder = builder.header("x-zeph-ibct", h);
        }
        builder
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    #[tokio::test]
    async fn no_keys_configured_bypasses_ibct_check() {
        let app = ibct_app(vec![]);
        let resp = app.oneshot(get_task_request(None)).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn missing_header_rejected_when_configured() {
        let app = ibct_app(vec![test_key()]);
        let resp = app.oneshot(get_task_request(None)).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn malformed_header_rejected() {
        let app = ibct_app(vec![test_key()]);
        let resp = app
            .oneshot(get_task_request(Some("not-valid-base64-json")))
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn valid_header_with_matching_task_id_accepted() {
        let key = test_key();
        let app = ibct_app(vec![key.clone()]);
        let token = Ibct::issue("task-1", TEST_ENDPOINT, Duration::from_mins(5), &key).unwrap();
        let resp = app
            .oneshot(get_task_request(Some(&token.encode().unwrap())))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn tampered_signature_rejected() {
        let key = test_key();
        let app = ibct_app(vec![key.clone()]);
        let mut token = Ibct::issue("task-1", TEST_ENDPOINT, Duration::from_mins(5), &key).unwrap();
        token.signature = "deadbeef".repeat(8);
        let resp = app
            .oneshot(get_task_request(Some(&token.encode().unwrap())))
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
    }

    #[tokio::test]
    async fn wrong_task_id_rejected() {
        let key = test_key();
        let app = ibct_app(vec![key.clone()]);
        let token = Ibct::issue("task-999", TEST_ENDPOINT, Duration::from_mins(5), &key).unwrap();
        let resp = app
            .oneshot(get_task_request(Some(&token.encode().unwrap())))
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
    }

    #[tokio::test]
    async fn wrong_endpoint_rejected() {
        let key = test_key();
        let app = ibct_app(vec![key.clone()]);
        let token = Ibct::issue(
            "task-1",
            "http://evil.example.com",
            Duration::from_mins(5),
            &key,
        )
        .unwrap();
        let resp = app
            .oneshot(get_task_request(Some(&token.encode().unwrap())))
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
    }

    #[tokio::test]
    async fn unknown_key_id_rejected() {
        let app = ibct_app(vec![test_key()]);
        let other_key = IbctKey {
            key_id: "k99".into(),
            key_bytes: b"other-secret".to_vec(),
        };
        let token =
            Ibct::issue("task-1", TEST_ENDPOINT, Duration::from_mins(5), &other_key).unwrap();
        let resp = app
            .oneshot(get_task_request(Some(&token.encode().unwrap())))
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
    }

    /// A brand-new task (no server-assigned `task_id` yet) is checked against the
    /// empty-string sentinel — the client cannot know the UUID the server will mint.
    #[tokio::test]
    async fn new_task_send_message_matches_empty_sentinel() {
        let key = test_key();
        let app = ibct_app(vec![key.clone()]);
        let token = Ibct::issue("", TEST_ENDPOINT, Duration::from_mins(5), &key).unwrap();
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": "1",
            "method": "message/send",
            "params": { "message": { "role": "user", "parts": [{"kind": "text", "text": "hi"}] } }
        });
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .header("x-zeph-ibct", token.encode().unwrap())
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn stream_endpoint_enforces_ibct_too() {
        let key = test_key();
        let app = ibct_app(vec![key]);
        let body = serde_json::json!({
            "params": { "message": { "role": "user", "parts": [{"kind": "text", "text": "hi"}] } }
        });
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a/stream")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    // Regression tests for #6260 review S1: the server's `expected_endpoint` is always the
    // pathless `card.url`, shared by both `/a2a` and `/a2a/stream`. A token scoped to a
    // pathful URL (the pre-fix client behavior) can never match it.

    /// Proves the exact bug class S1 identified: a token scoped to a full per-route POST URL
    /// (including the path) does not match the server's pathless `card.url` and is rejected.
    /// If `A2aClient::ibct_header_value` ever regresses to passing its raw `endpoint` argument
    /// straight into `Ibct::issue` instead of normalizing via `ibct_scope_origin`, every real
    /// request it sends would fail exactly like this.
    #[tokio::test]
    async fn pathful_endpoint_scope_is_rejected() {
        let key = test_key();
        let app = ibct_app(vec![key.clone()]);
        let pathful_endpoint = format!("{TEST_ENDPOINT}/a2a");
        let token = Ibct::issue("task-1", &pathful_endpoint, Duration::from_mins(5), &key).unwrap();
        let resp = app
            .oneshot(get_task_request(Some(&token.encode().unwrap())))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            403,
            "a token scoped to a pathful endpoint must not match the server's pathless card.url"
        );
    }

    /// A token correctly scoped to the origin shared by both routes (what
    /// `A2aClient::ibct_scope_origin` now produces for any full per-route URL) must be
    /// accepted on `/a2a` and, independently, on `/a2a/stream` — proving the server's own
    /// verification is route-agnostic, the necessary counterpart to the client-side fix
    /// (unit-tested directly in `client.rs`'s `ibct_scope_origin_*` tests).
    #[tokio::test]
    async fn origin_scoped_token_accepted_on_both_routes() {
        let key = test_key();

        let get_app = ibct_app(vec![key.clone()]);
        let token_a2a = Ibct::issue("task-1", TEST_ENDPOINT, Duration::from_mins(5), &key).unwrap();
        let resp = get_app
            .oneshot(get_task_request(Some(&token_a2a.encode().unwrap())))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "/a2a must accept an origin-scoped token"
        );

        let stream_app = ibct_app(vec![key.clone()]);
        let token_stream = Ibct::issue("", TEST_ENDPOINT, Duration::from_mins(5), &key).unwrap();
        let stream_body = serde_json::json!({
            "params": { "message": { "role": "user", "parts": [{"kind": "text", "text": "hi"}] } }
        });
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a/stream")
            .header("content-type", "application/json")
            .header("x-zeph-ibct", token_stream.encode().unwrap())
            .body(Body::from(serde_json::to_vec(&stream_body).unwrap()))
            .unwrap();
        let resp = stream_app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            200,
            "/a2a/stream must accept the SAME origin-scoped endpoint value as /a2a"
        );
    }

    /// `IbctConfig`'s body-peek bound must come from the configured `max_body_size`, not a
    /// fixed constant — otherwise a legitimate large-but-allowed request that passes the outer
    /// `RequestBodyLimitLayer` could still be spuriously rejected by `ibct_middleware`'s own
    /// buffering (#6260 review M5).
    #[tokio::test]
    async fn ibct_body_peek_respects_configured_max_body_size() {
        let key = test_key();
        let small_max_body_size = 64;
        // Outer RequestBodyLimitLayer stays generous (1 MiB) so only ibct_middleware's own
        // to_bytes bound (small_max_body_size) can be the thing that rejects the request.
        let app = build_router_with_full_config(
            test_state(),
            AuthConfig::new(None, false),
            0,
            1024 * 1024,
            IbctConfig::new(vec![key.clone()], TEST_ENDPOINT, small_max_body_size),
        );

        let token = Ibct::issue("task-1", TEST_ENDPOINT, Duration::from_mins(5), &key).unwrap();
        // A body comfortably larger than `small_max_body_size` but well under the outer
        // RequestBodyLimitLayer's 1 MiB — must be rejected by ibct_middleware's own buffering.
        let oversized_params = serde_json::json!({"id": "x".repeat(200)});
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": "1",
            "method": "tasks/get", "params": oversized_params
        });
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .header("x-zeph-ibct", token.encode().unwrap())
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            400,
            "ibct_middleware's to_bytes must be bounded by IbctConfig::max_body_size"
        );
    }

    /// Regression test for #6260 review M6: `A2aServer::serve()` now runs the server's own
    /// `card.url` through `ibct_scope_origin` — the same normalization the client applies to
    /// its `endpoint` argument — before constructing `IbctConfig`. Without that, a
    /// non-canonical `public_url` (trailing slash, a stray path, or an explicit default port)
    /// would silently 403 every request even from a correctly-behaving, origin-scoped client,
    /// reintroducing the S1 bug class on the server side. This exercises the real
    /// `ibct_scope_origin` function on both the "card.url" input and the "client endpoint"
    /// input, mirroring exactly what `serve()` and `ibct_header_value` each do.
    #[tokio::test]
    async fn non_canonical_card_url_still_matches_client_issued_token() {
        let key = test_key();
        let client_endpoint = ibct_scope_origin("http://localhost:8080/a2a");

        for non_canonical_card_url in [
            "http://localhost:8080/",    // trailing slash
            "http://localhost:8080/a2a", // stray path (misconfigured public_url)
            "HTTP://LOCALHOST:8080",     // uppercase scheme/host
        ] {
            let normalized_card_url = ibct_scope_origin(non_canonical_card_url);
            assert_eq!(
                normalized_card_url, TEST_ENDPOINT,
                "ibct_scope_origin({non_canonical_card_url:?}) should normalize to {TEST_ENDPOINT:?}"
            );

            let app = build_router_with_full_config(
                test_state(),
                AuthConfig::new(None, false),
                0,
                DEFAULT_MAX_BODY_SIZE,
                IbctConfig::new(
                    vec![key.clone()],
                    normalized_card_url,
                    DEFAULT_MAX_BODY_SIZE,
                ),
            );

            let token =
                Ibct::issue("task-1", &client_endpoint, Duration::from_mins(5), &key).unwrap();
            let resp = app
                .oneshot(get_task_request(Some(&token.encode().unwrap())))
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                200,
                "non-canonical card.url {non_canonical_card_url:?} must still match an \
                 origin-scoped client token after normalization"
            );
        }
    }
}
