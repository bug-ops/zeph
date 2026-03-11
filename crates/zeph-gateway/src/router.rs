// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

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

use super::handlers::{health_handler, webhook_handler};
use super::server::AppState;

#[derive(Clone)]
struct AuthConfig {
    token_hash: Option<blake3::Hash>,
}

const MAX_RATE_LIMIT_ENTRIES: usize = 10_000;
const RATE_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct RateLimitState {
    limit: u32,
    counters: Arc<Mutex<HashMap<IpAddr, (u32, Instant)>>>,
}

pub(crate) fn build_router(
    state: AppState,
    auth_token: Option<&str>,
    rate_limit: u32,
    max_body_size: usize,
) -> Router {
    let auth_cfg = AuthConfig {
        token_hash: auth_token.map(|t| blake3::hash(t.as_bytes())),
    };
    let rate_state = RateLimitState {
        limit: rate_limit,
        counters: Arc::new(Mutex::new(HashMap::new())),
    };

    let protected = Router::new()
        .route("/webhook", post(webhook_handler))
        .layer(middleware::from_fn_with_state(
            rate_state,
            rate_limit_middleware,
        ))
        .layer(middleware::from_fn_with_state(auth_cfg, auth_middleware))
        .layer(RequestBodyLimitLayer::new(max_body_size));

    Router::new()
        .route("/health", get(health_handler))
        .merge(protected)
        .with_state(state)
}

async fn auth_middleware(
    axum::extract::State(cfg): axum::extract::State<AuthConfig>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if let Some(expected_hash) = cfg.token_hash {
        let auth_header = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok());

        let token = auth_header
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");

        // Hash the submitted token to a fixed-length digest before comparing.
        // Expected token hash is pre-computed at startup (stored in AuthConfig).
        // ct_eq operates on two 32-byte arrays — constant time regardless of content.
        let token_hash = blake3::hash(token.as_bytes());
        if !bool::from(token_hash.as_bytes().ct_eq(expected_hash.as_bytes())) {
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

    let ip = req
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), |ci| ci.0.ip());

    let now = Instant::now();
    let mut counters = state.counters.lock().await;

    if counters.len() >= MAX_RATE_LIMIT_ENTRIES && !counters.contains_key(&ip) {
        counters.retain(|_, (_, ts)| now.duration_since(*ts) < RATE_WINDOW);
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
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;
    use crate::server::AppState;

    fn test_state() -> (AppState, tokio::sync::mpsc::Receiver<String>) {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let state = AppState {
            webhook_tx: tx,
            started_at: Instant::now(),
        };
        (state, rx)
    }

    fn make_router(
        auth: Option<&str>,
        rate_limit: u32,
    ) -> (Router, tokio::sync::mpsc::Receiver<String>) {
        let (state, rx) = test_state();
        (build_router(state, auth, rate_limit, 1_048_576), rx)
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let (app, _rx) = make_router(None, 0);
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test]
    async fn webhook_accepted() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let state = AppState {
            webhook_tx: tx,
            started_at: Instant::now(),
        };
        let app = build_router(state, None, 0, 1_048_576);

        let body = serde_json::json!({
            "channel": "discord",
            "sender": "user1",
            "body": "hello"
        });
        let req = Request::builder()
            .method("POST")
            .uri("/webhook")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let msg = rx.try_recv().unwrap();
        assert!(msg.contains("user1"));
    }

    #[tokio::test]
    async fn auth_rejects_missing_token() {
        let (app, _rx) = make_router(Some("secret"), 0);
        let body = serde_json::json!({"channel":"a","sender":"b","body":"c"});
        let req = Request::builder()
            .method("POST")
            .uri("/webhook")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn auth_accepts_valid_token() {
        let (app, _rx) = make_router(Some("secret"), 0);
        let body = serde_json::json!({"channel":"a","sender":"b","body":"c"});
        let req = Request::builder()
            .method("POST")
            .uri("/webhook")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn auth_rejects_wrong_token() {
        let (app, _rx) = make_router(Some("secret"), 0);
        let body = serde_json::json!({"channel":"a","sender":"b","body":"c"});
        let req = Request::builder()
            .method("POST")
            .uri("/webhook")
            .header("content-type", "application/json")
            .header("authorization", "Bearer wrong")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn health_skips_auth() {
        let (app, _rx) = make_router(Some("secret"), 0);
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn rate_limit_enforced() {
        use tower::Service;

        let (mut app, _rx) = make_router(None, 2);
        let make_req = || {
            let body = serde_json::json!({"channel":"a","sender":"b","body":"c"});
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap()
        };

        let resp = app.call(make_req()).await.unwrap();
        assert_eq!(resp.status(), 200);
        let resp = app.call(make_req()).await.unwrap();
        assert_eq!(resp.status(), 200);
        let resp = app.call(make_req()).await.unwrap();
        assert_eq!(resp.status(), 429);
    }

    #[tokio::test]
    async fn no_auth_when_token_unset() {
        let (app, _rx) = make_router(None, 0);
        let body = serde_json::json!({"channel": "a", "sender": "b", "body": "c"});
        let req = Request::builder()
            .method("POST")
            .uri("/webhook")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn body_size_limit() {
        let (state, _rx) = test_state();
        let app = build_router(state, None, 0, 64);
        let oversized = vec![b'a'; 128];
        let req = Request::builder()
            .method("POST")
            .uri("/webhook")
            .header("content-type", "application/json")
            .body(Body::from(oversized))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 413);
    }

    /// Statistical timing test for SEC-M22-001.
    ///
    /// Verifies that `ct_eq` comparison time is constant regardless of token length.
    /// Both inputs are hashed to 32-byte BLAKE3 digests before comparison, so
    /// `ct_eq` always operates on identically-sized arrays — timing must not vary.
    #[test]
    fn bearer_ct_eq_is_constant_time() {
        use std::time::Instant;

        const ITERS: u32 = 100_000;
        // Max allowed ratio between slowest and fastest measurement (10× is very conservative;
        // in practice the ratio is < 2× on any machine, but CI can be noisy).
        const MAX_RATIO: u128 = 10;

        let expected_hash = blake3::hash(b"super-secret-gateway-token");

        // Tokens of vastly different lengths whose hashes are all wrong (→ ct_eq returns false).
        let candidates: &[&[u8]] = &[b"x", b"wrong_token_123", &[b'z'; 512]];
        let mut times_ns: Vec<u128> = Vec::with_capacity(candidates.len());

        for candidate in candidates {
            let h = blake3::hash(candidate);
            // Warm up to avoid first-call JIT / cache effects.
            for _ in 0..1_000 {
                let _ = h.as_bytes().ct_eq(expected_hash.as_bytes());
            }
            let start = Instant::now();
            for _ in 0..ITERS {
                let _ = h.as_bytes().ct_eq(expected_hash.as_bytes());
            }
            times_ns.push(start.elapsed().as_nanos() / u128::from(ITERS));
        }

        let min = *times_ns.iter().min().unwrap();
        let max = *times_ns.iter().max().unwrap();
        assert!(
            min > 0 && max / min < MAX_RATIO,
            "ct_eq timing ratio {max}/{min} exceeds {MAX_RATIO}×; times per iter: {times_ns:?} ns"
        );
    }
}
