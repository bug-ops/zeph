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

/// Pre-computed authentication configuration for the bearer-token middleware.
///
/// The expected token is hashed once at startup so that the per-request
/// comparison always operates on two 32-byte BLAKE3 digests, keeping the
/// comparison both O(1) and constant-time.
#[derive(Clone)]
struct AuthConfig {
    /// BLAKE3 hash of the configured bearer token, or `None` when auth is disabled.
    token_hash: Option<blake3::Hash>,
}

/// Maximum number of IP entries retained in the rate-limit map before a GC pass.
///
/// When the map reaches this size and a new, unseen IP arrives, expired entries
/// (older than [`RATE_WINDOW`]) are evicted before inserting the new one.  This
/// bounds memory usage to roughly `MAX_RATE_LIMIT_ENTRIES * ~56 bytes`.
const MAX_RATE_LIMIT_ENTRIES: usize = 10_000;

/// Fixed window duration for the per-IP request counter.
const RATE_WINDOW: Duration = Duration::from_mins(1);

/// Shared state threaded through the rate-limiting middleware.
#[derive(Clone)]
struct RateLimitState {
    /// Maximum number of requests allowed per IP in one [`RATE_WINDOW`].
    /// `0` means rate limiting is disabled.
    limit: u32,
    /// Map from remote IP to `(request_count, window_start)`.
    counters: Arc<Mutex<HashMap<IpAddr, (u32, Instant)>>>,
}

/// Build the complete axum [`Router`] for the gateway.
///
/// Routes:
/// - `GET /health` — unauthenticated liveness check ([`health_handler`])
/// - `POST /webhook` — authenticated, rate-limited, body-size-limited ingestion
///   ([`webhook_handler`])
///
/// Middleware stack applied to `/webhook` (outermost → innermost):
/// 1. [`RequestBodyLimitLayer`] — rejects bodies larger than `max_body_size`
/// 2. [`auth_middleware`] — constant-time bearer-token check
/// 3. [`rate_limit_middleware`] — per-IP fixed-window counter
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

/// Axum middleware that enforces bearer-token authentication.
///
/// When [`AuthConfig::token_hash`] is `Some`, the request must contain an
/// `Authorization: Bearer <token>` header whose value, when hashed with BLAKE3,
/// matches the pre-computed digest.  Comparison uses [`ConstantTimeEq`] on the
/// two fixed-length 32-byte arrays so that the comparison time is independent
/// of the token content, preventing timing-oracle attacks.
///
/// Requests without a valid token receive `401 Unauthorized`.
/// When auth is not configured (`token_hash` is `None`) all requests pass through.
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

/// Axum middleware that enforces a per-IP fixed-window rate limit.
///
/// Each remote IP is tracked independently.  A counter is incremented on every
/// request within the current window.  When the counter exceeds
/// [`RateLimitState::limit`] the request receives `429 Too Many Requests`.
///
/// The window resets when [`RATE_WINDOW`] has elapsed since the first request in
/// the current window.  When [`RateLimitState::limit`] is `0` the middleware
/// passes all requests through without tracking.
///
/// To prevent unbounded memory growth, expired entries are evicted when the
/// counters map reaches [`MAX_RATE_LIMIT_ENTRIES`] and a new IP is encountered.
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
    async fn webhook_missing_field_returns_json_error() {
        let (app, _rx) = make_router(None, 0);
        // Missing "sender" field
        let body = serde_json::json!({"channel": "ci643", "body": "test"});
        let req = Request::builder()
            .method("POST")
            .uri("/webhook")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 422);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("application/json"),
            "expected JSON content-type, got: {ct}"
        );
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("error").is_some());
        assert_eq!(json["status"], 422);
    }

    #[tokio::test]
    async fn webhook_validation_failure_returns_json_error() {
        let (app, _rx) = make_router(None, 0);
        let body = serde_json::json!({
            "channel": "ci643",
            "sender": "a".repeat(257),
            "body": "hello"
        });
        let req = Request::builder()
            .method("POST")
            .uri("/webhook")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 422);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("application/json"),
            "expected JSON content-type, got: {ct}"
        );
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("error").is_some());
        assert_eq!(json["status"], 422);
    }

    #[tokio::test]
    async fn webhook_503_returns_json_error() {
        // Build a state whose channel is already closed (rx dropped) so that
        // the send in webhook_handler will fail immediately.
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(1);
        drop(rx);
        let state = AppState {
            webhook_tx: tx,
            started_at: Instant::now(),
        };
        let app = build_router(state, None, 0, 1_048_576);

        let body = serde_json::json!({"channel": "c", "sender": "s", "body": "b"});
        let req = Request::builder()
            .method("POST")
            .uri("/webhook")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 503);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("application/json"),
            "expected application/json content-type for 503, got: {ct}"
        );
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["status"], 503);
        assert!(json.get("error").is_some());
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
