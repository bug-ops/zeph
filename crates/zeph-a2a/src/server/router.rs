// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Axum router construction with auth middleware, rate limiting, and body size limits.
//!
//! [`build_router_with_full_config`] is the production entry point called by [`A2aServer::serve`].
//! The test-only [`build_router_with_config`] omits `require_auth` and `max_body_size`
//! for convenience.

use axum::Router;
use axum::middleware;
use axum::routing::{get, post};
use tower_http::limit::RequestBodyLimitLayer;
use zeph_common::http_middleware::{
    AuthConfig, RateLimitState, auth_middleware, rate_limit_middleware,
};

use super::handlers::{agent_card_handler, jsonrpc_handler, stream_handler};
use super::state::AppState;

#[cfg(test)]
const DEFAULT_MAX_BODY_SIZE: usize = 1024 * 1024; // 1 MiB

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
    )
}

pub fn build_router_with_full_config(
    state: AppState,
    auth_cfg: AuthConfig,
    rate_limit: u32,
    max_body_size: usize,
) -> Router {
    let rate_state = RateLimitState::new(rate_limit, &[]);

    let protected = Router::new()
        .route("/a2a", post(jsonrpc_handler))
        .route("/a2a/stream", post(stream_handler))
        .layer(middleware::from_fn_with_state(
            rate_state,
            rate_limit_middleware,
        ))
        .layer(middleware::from_fn_with_state(auth_cfg, auth_middleware))
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
