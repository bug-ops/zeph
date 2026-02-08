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

const MAX_BODY_SIZE: usize = 1024 * 1024; // 1 MiB

#[derive(Clone)]
struct AuthConfig {
    token: Option<String>,
}

#[derive(Clone)]
struct RateLimitState {
    limit: u32,
    counters: Arc<Mutex<HashMap<IpAddr, (u32, Instant)>>>,
}

pub fn build_router_with_config(
    state: AppState,
    auth_token: Option<String>,
    rate_limit: u32,
) -> Router {
    let auth_cfg = AuthConfig { token: auth_token };
    let rate_state = RateLimitState {
        limit: rate_limit,
        counters: Arc::new(Mutex::new(HashMap::new())),
    };

    let protected = Router::new()
        .route("/a2a", post(jsonrpc_handler))
        .route("/a2a/stream", post(stream_handler))
        .layer(middleware::from_fn_with_state(
            rate_state,
            rate_limit_middleware,
        ))
        .layer(middleware::from_fn_with_state(auth_cfg, auth_middleware))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_SIZE));

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
    let window = Duration::from_secs(60);

    let mut counters = state.counters.lock().await;
    let entry = counters.entry(ip).or_insert((0, now));

    if now.duration_since(entry.1) >= window {
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

        let oversized = vec![b'a'; MAX_BODY_SIZE + 1];
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
}
