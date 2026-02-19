use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;
use tower_http::limit::RequestBodyLimitLayer;

use super::handlers::{agent_card_handler, jsonrpc_handler, stream_handler};
use super::state::AppState;

#[cfg(test)]
const DEFAULT_MAX_BODY_SIZE: usize = 1024 * 1024; // 1 MiB

#[derive(Clone)]
struct AuthConfig {
    token: Option<String>,
}

const MAX_RATE_LIMIT_ENTRIES: usize = 10_000;
const EVICTION_INTERVAL: Duration = Duration::from_secs(60);
const RATE_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct RateLimitState {
    limit: u32,
    counters: Arc<Mutex<HashMap<IpAddr, (u32, Instant)>>>,
}

fn spawn_eviction_task(counters: Arc<Mutex<HashMap<IpAddr, (u32, Instant)>>>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(EVICTION_INTERVAL);
        interval.tick().await;
        loop {
            interval.tick().await;
            let now = Instant::now();
            let mut map = counters.lock().await;
            map.retain(|_, (_, ts)| now.duration_since(*ts) < RATE_WINDOW);
        }
    });
}

#[cfg(test)]
pub fn build_router_with_config(
    state: AppState,
    auth_token: Option<String>,
    rate_limit: u32,
) -> Router {
    build_router_with_full_config(state, auth_token, rate_limit, DEFAULT_MAX_BODY_SIZE)
}

pub fn build_router_with_full_config(
    state: AppState,
    auth_token: Option<String>,
    rate_limit: u32,
    max_body_size: usize,
) -> Router {
    let auth_cfg = AuthConfig { token: auth_token };
    let counters = Arc::new(Mutex::new(HashMap::new()));
    if rate_limit > 0 {
        spawn_eviction_task(Arc::clone(&counters));
    }
    let rate_state = RateLimitState {
        limit: rate_limit,
        counters,
    };

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
        .route("/.well-known/agent-card.json", get(agent_card_handler))
        .merge(protected)
        .with_state(state)
}

async fn auth_middleware(
    axum::extract::State(cfg): axum::extract::State<AuthConfig>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if let Some(ref expected) = cfg.token {
        let auth_header = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok());

        let token = auth_header
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");

        if token.len() != expected.len() || !bool::from(token.as_bytes().ct_eq(expected.as_bytes()))
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }

    next.run(req).await
}

async fn rate_limit_middleware(
    axum::extract::State(state): axum::extract::State<RateLimitState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if state.limit == 0 {
        return next.run(req).await;
    }

    // Extract IP from ConnectInfo if available, fall back to 0.0.0.0
    let ip = req
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), |ci| ci.0.ip());

    let now = Instant::now();

    let mut counters = state.counters.lock().await;

    if counters.len() >= MAX_RATE_LIMIT_ENTRIES && !counters.contains_key(&ip) {
        let before_eviction = counters.len();
        counters.retain(|_, (_, ts)| now.duration_since(*ts) < RATE_WINDOW);
        let after_eviction = counters.len();

        if after_eviction >= MAX_RATE_LIMIT_ENTRIES {
            tracing::warn!(
                before = before_eviction,
                after = after_eviction,
                limit = MAX_RATE_LIMIT_ENTRIES,
                "rate limiter at capacity after stale entry eviction, rejecting new IP"
            );
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
    }

    let entry = counters.entry(ip).or_insert((0, now));

    if now.duration_since(entry.1) >= RATE_WINDOW {
        *entry = (1, now);
    } else {
        entry.0 += 1;
        if entry.0 > state.limit {
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
    }
    drop(counters);

    next.run(req).await
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use tower::ServiceExt;

    use super::*;
    use crate::server::testing::test_state;

    #[tokio::test]
    async fn auth_allows_valid_token() {
        let app = build_router_with_config(test_state(), Some("secret-token".into()), 0);

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
        let app = build_router_with_config(test_state(), Some("secret-token".into()), 0);

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
        let app = build_router_with_config(test_state(), Some("secret-token".into()), 0);

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
        let app = build_router_with_config(test_state(), Some("secret-token".into()), 0);

        let req = axum::http::Request::builder()
            .uri("/.well-known/agent-card.json")
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
        let app = build_router_with_config(test_state(), Some("secret".into()), 0);

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
        let app = build_router_with_config(test_state(), Some("secret".into()), 0);

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
    async fn max_entries_cap_rejects_when_all_entries_fresh() {
        // Fill map with fresh entries (within RATE_WINDOW) so retain() keeps them all.
        // After retain() the map is still at capacity, so the middleware returns 429.
        let counters = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut map = counters.lock().await;
            let fresh = Instant::now();
            for i in 0..MAX_RATE_LIMIT_ENTRIES {
                let ip = IpAddr::V4(std::net::Ipv4Addr::new(
                    ((i >> 16) & 0xFF) as u8,
                    ((i >> 8) & 0xFF) as u8,
                    (i & 0xFF) as u8,
                    1,
                ));
                map.insert(ip, (1, fresh));
            }
            assert_eq!(map.len(), MAX_RATE_LIMIT_ENTRIES);
        }

        let new_ip = IpAddr::V4(std::net::Ipv4Addr::new(255, 255, 255, 255));

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
            let stale = Instant::now() - Duration::from_secs(120);
            for i in 0..MAX_RATE_LIMIT_ENTRIES {
                let ip = IpAddr::V4(std::net::Ipv4Addr::new(
                    ((i >> 16) & 0xFF) as u8,
                    ((i >> 8) & 0xFF) as u8,
                    (i & 0xFF) as u8,
                    1,
                ));
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
        let stale_time = Instant::now() - Duration::from_secs(120);
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
}
