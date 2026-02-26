// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

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
            // Constant-time comparison to prevent timing attacks.
            .is_some_and(|provided| provided.as_bytes().ct_eq(expected.as_bytes()).into());

        if authorized {
            let fut = self.inner.call(req);
            Box::pin(fut)
        } else {
            Box::pin(async move { Ok(StatusCode::UNAUTHORIZED.into_response()) })
        }
    }
}
