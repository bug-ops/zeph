// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use axum::Router;
use axum::middleware;
use axum::routing::{get, post};
use tower_http::limit::RequestBodyLimitLayer;
use zeph_common::http_middleware::{
    AuthConfig, RateLimitState, auth_middleware, rate_limit_middleware,
};

use super::handlers::{health_handler, webhook_handler};
use super::server::AppState;

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
    trusted_proxy_cidrs: &[String],
) -> Router {
    let auth_cfg = AuthConfig::new(auth_token, false);
    let rate_state = RateLimitState::new(rate_limit, trusted_proxy_cidrs);

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

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::{Service, ServiceExt};

    use super::*;
    use crate::server::AppState;

    fn test_state() -> (AppState, tokio::sync::mpsc::Receiver<String>) {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let state = AppState {
            webhook_tx: tx,
            started_at: Instant::now(),
            webhook_send_timeout: std::time::Duration::from_secs(5),
        };
        (state, rx)
    }

    fn make_router(
        auth: Option<&str>,
        rate_limit: u32,
    ) -> (Router, tokio::sync::mpsc::Receiver<String>) {
        let (state, rx) = test_state();
        (build_router(state, auth, rate_limit, 1_048_576, &[]), rx)
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
            webhook_send_timeout: std::time::Duration::from_secs(5),
        };
        let app = build_router(state, None, 0, 1_048_576, &[]);

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
            webhook_send_timeout: std::time::Duration::from_secs(5),
        };
        let app = build_router(state, None, 0, 1_048_576, &[]);

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
        let app = build_router(state, None, 0, 64, &[]);
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

    // ── XFF rightmost-untrusted tests (#3909 regression) ────────────────────
    // CIDR and ct_eq unit tests live in zeph-common::http_middleware::tests.

    #[tokio::test]
    async fn xff_rightmost_untrusted_selected() {
        // Trusted proxy: 10.0.0.1. XFF: "1.2.3.4, 10.0.0.1".
        // Rate-limit counter should key on 1.2.3.4 (rightmost untrusted).
        let (state, _rx) = test_state();
        let cidrs = vec!["0.0.0.0/0".to_string()];
        let mut app = build_router(state, None, 1, 1_048_576, &cidrs);

        let make_req = || {
            let body = serde_json::json!({"channel":"a","sender":"b","body":"c"});
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("content-type", "application/json")
                .header("x-forwarded-for", "1.2.3.4, 10.0.0.1")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap()
        };

        let resp1 = app.call(make_req()).await.unwrap();
        assert_eq!(resp1.status(), 200);
        let resp2 = app.call(make_req()).await.unwrap();
        assert_eq!(
            resp2.status(),
            429,
            "second request from same real IP must be rate-limited"
        );
    }

    #[tokio::test]
    async fn xff_absent_falls_back_to_tcp_peer() {
        // No trusted CIDRs → ignores XFF, uses peer IP for rate limiting.
        let (state, _rx) = test_state();
        let mut app = build_router(state, None, 1, 1_048_576, &[]);

        let make_req = || {
            let body = serde_json::json!({"channel":"a","sender":"b","body":"c"});
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap()
        };

        let resp1 = app.call(make_req()).await.unwrap();
        assert_eq!(resp1.status(), 200);
        let resp2 = app.call(make_req()).await.unwrap();
        assert_eq!(
            resp2.status(),
            429,
            "second request must be rate-limited via TCP peer"
        );
    }

    #[tokio::test]
    async fn xff_all_trusted_falls_back_to_peer() {
        // All IPs in XFF are trusted → no untrusted entry found → fall back to TCP peer.
        let (state, rx) = test_state();
        let cidrs = vec!["0.0.0.0/0".to_string()];
        let app = build_router(state, None, 0, 1_048_576, &cidrs);

        let body = serde_json::json!({"channel":"a","sender":"b","body":"c"});
        let req = Request::builder()
            .method("POST")
            .uri("/webhook")
            .header("content-type", "application/json")
            .header("x-forwarded-for", "10.0.0.1, 10.0.0.2")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        drop(rx);
    }
}
