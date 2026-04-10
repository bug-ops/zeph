// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bearer token authentication middleware for the ACP HTTP transport.
//!
//! Token comparison uses `blake3` hashing + `subtle::ConstantTimeEq` to prevent
//! timing side-channels. Both the provided and expected tokens are hashed to
//! fixed-length digests before comparison, eliminating the length side-channel
//! present in direct byte comparison.

use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use axum::response::IntoResponse;
use subtle::ConstantTimeEq as _;
use tower::{Layer, Service};

/// Tower middleware layer that validates `Authorization: Bearer <token>` headers
/// using constant-time comparison to prevent timing attacks.
#[derive(Clone)]
pub(crate) struct BearerAuthLayer {
    token: String,
}

impl BearerAuthLayer {
    pub(crate) fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
        }
    }
}

impl<S> Layer<S> for BearerAuthLayer {
    type Service = BearerAuthMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        BearerAuthMiddleware {
            inner,
            token: self.token.clone(),
        }
    }
}

/// Middleware service that enforces bearer token authentication.
#[derive(Clone)]
pub(crate) struct BearerAuthMiddleware<S> {
    inner: S,
    token: String,
}

impl<S> Service<Request<Body>> for BearerAuthMiddleware<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let expected = self.token.clone();

        let authorized = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            // Hash both sides first so ct_eq operates on fixed-length digests,
            // eliminating the length side-channel present in direct byte comparison.
            .is_some_and(|provided| {
                let h_provided = blake3::hash(provided.as_bytes());
                let h_expected = blake3::hash(expected.as_bytes());
                h_provided.as_bytes().ct_eq(h_expected.as_bytes()).into()
            });

        if authorized {
            let fut = self.inner.call(req);
            Box::pin(fut)
        } else {
            Box::pin(async move { Ok(StatusCode::UNAUTHORIZED.into_response()) })
        }
    }
}

#[cfg(all(test, feature = "acp-http"))]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt as _;

    use super::*;

    fn ok_handler() -> axum::Router {
        axum::Router::new().route("/", get(|| async { StatusCode::OK }))
    }

    fn app_with_token(token: &str) -> axum::Router {
        ok_handler().layer(BearerAuthLayer::new(token))
    }

    async fn send(app: axum::Router, auth: Option<&str>) -> StatusCode {
        let mut builder = Request::builder().method("GET").uri("/");
        if let Some(v) = auth {
            builder = builder.header("authorization", v);
        }
        let req = builder.body(Body::empty()).unwrap();
        app.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn correct_token_accepted() {
        let app = app_with_token("my-secret");
        assert_eq!(send(app, Some("Bearer my-secret")).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn wrong_token_rejected() {
        let app = app_with_token("my-secret");
        assert_eq!(
            send(app, Some("Bearer wrong")).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn empty_token_rejected() {
        let app = app_with_token("my-secret");
        assert_eq!(send(app, Some("Bearer ")).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn missing_header_rejected() {
        let app = app_with_token("my-secret");
        assert_eq!(send(app, None).await, StatusCode::UNAUTHORIZED);
    }
}
