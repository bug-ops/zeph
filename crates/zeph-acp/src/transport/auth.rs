// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bearer token authentication middleware for the ACP HTTP transport.
//!
//! Token comparison uses `blake3` hashing + `subtle::ConstantTimeEq` to prevent
//! timing side-channels. Both the provided and expected tokens are hashed to
//! fixed-length digests before comparison, eliminating the length side-channel
//! present in direct byte comparison.
//!
//! Supports multiple named clients (#5868): each configured
//! [`AcpClientToken`] authenticates its own token, and on
//! match the matched client's `id` is injected as [`TokenIdentity`] into the request's
//! extensions — downstream handlers read it to derive the connection's `owner_key` for ACP
//! session-persistence scoping.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use axum::response::IntoResponse;
use subtle::ConstantTimeEq as _;
use tower::{Layer, Service};

use crate::transport::AcpClientToken;

/// Authenticated client identity, injected into request extensions by `BearerAuthLayer` on
/// a successful bearer-token match. Absent when no auth layer is applied (empty client list).
///
/// `pub` (not `pub(crate)`) solely so it can appear as an `axum` extractor parameter type on
/// the `pub` HTTP handler functions in this crate; it is not part of the crate's documented
/// public API (not re-exported at the crate root).
#[derive(Clone, Debug)]
pub struct TokenIdentity(pub(crate) String);

/// Tower middleware layer that validates `Authorization: Bearer <token>` headers against a
/// named-client credential set, using constant-time comparison to prevent timing attacks.
#[derive(Clone)]
pub(crate) struct BearerAuthLayer {
    clients: Arc<Vec<AcpClientToken>>,
}

impl BearerAuthLayer {
    /// Constructs the layer from `clients`, dropping any entry whose `token` is empty or
    /// whitespace-only.
    ///
    /// An empty token would hash to `blake3::hash(b"")` and match a request presenting
    /// `Authorization: Bearer ` (empty bearer value) — this is the last line of defense
    /// against that bypass, independent of whatever validated the tokens upstream (#6270).
    /// Trims before checking (F5) to stay consistent with `resolve_acp_auth_clients`'s
    /// existing `!t.trim().is_empty()` check — a whitespace-only token is as weak a secret
    /// as an empty one, even though it does not collide with the missing-header default.
    pub(crate) fn new(clients: Vec<AcpClientToken>) -> Self {
        let clients = clients
            .into_iter()
            .filter(|c| {
                let keep = !c.token.trim().is_empty();
                if !keep {
                    tracing::warn!(id = %c.id, "BearerAuthLayer: dropping client with empty token");
                }
                keep
            })
            .collect();
        Self {
            clients: Arc::new(clients),
        }
    }
}

impl<S> Layer<S> for BearerAuthLayer {
    type Service = BearerAuthMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        BearerAuthMiddleware {
            inner,
            clients: Arc::clone(&self.clients),
        }
    }
}

/// Middleware service that enforces bearer token authentication.
#[derive(Clone)]
pub(crate) struct BearerAuthMiddleware<S> {
    inner: S,
    clients: Arc<Vec<AcpClientToken>>,
}

/// Find the first configured client whose token matches `provided`, in configured order.
/// Every candidate is compared via hashed constant-time equality (no early exit on token
/// bytes) — only the outer `find` short-circuits on which candidate matched, which config
/// validation (unique tokens) already makes a non-issue in practice.
fn match_client<'a>(clients: &'a [AcpClientToken], provided: &str) -> Option<&'a AcpClientToken> {
    let h_provided = blake3::hash(provided.as_bytes());
    clients.iter().find(|c| {
        let h_expected = blake3::hash(c.token.as_bytes());
        bool::from(h_provided.as_bytes().ct_eq(h_expected.as_bytes()))
    })
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

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        let matched = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .and_then(|provided| match_client(&self.clients, provided))
            .map(|c| c.id.clone());

        if let Some(id) = matched {
            req.extensions_mut().insert(TokenIdentity(id));
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

    fn client(id: &str, token: &str) -> AcpClientToken {
        AcpClientToken {
            id: id.to_owned(),
            token: token.to_owned(),
        }
    }

    fn ok_handler() -> axum::Router {
        axum::Router::new().route(
            "/",
            get(|req: Request<Body>| async move {
                req.extensions()
                    .get::<TokenIdentity>()
                    .map_or_else(String::new, |id| id.0.clone())
            }),
        )
    }

    fn app_with_clients(clients: Vec<AcpClientToken>) -> axum::Router {
        ok_handler().layer(BearerAuthLayer::new(clients))
    }

    async fn send(app: axum::Router, auth: Option<&str>) -> (StatusCode, String) {
        let mut builder = Request::builder().method("GET").uri("/");
        if let Some(v) = auth {
            builder = builder.header("authorization", v);
        }
        let req = builder.body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn correct_token_accepted_and_identifies_client() {
        let app = app_with_clients(vec![client("default", "my-secret")]);
        let (status, body) = send(app, Some("Bearer my-secret")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "default");
    }

    #[tokio::test]
    async fn wrong_token_rejected() {
        let app = app_with_clients(vec![client("default", "my-secret")]);
        assert_eq!(
            send(app, Some("Bearer wrong")).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn empty_token_rejected() {
        let app = app_with_clients(vec![client("default", "my-secret")]);
        assert_eq!(send(app, Some("Bearer ")).await.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn missing_header_rejected() {
        let app = app_with_clients(vec![client("default", "my-secret")]);
        assert_eq!(send(app, None).await.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn multi_client_each_token_identifies_its_own_client() {
        let app = app_with_clients(vec![client("alice", "token-a"), client("bob", "token-b")]);
        let (status_a, body_a) = send(app.clone(), Some("Bearer token-a")).await;
        assert_eq!(status_a, StatusCode::OK);
        assert_eq!(body_a, "alice");

        let (status_b, body_b) = send(app, Some("Bearer token-b")).await;
        assert_eq!(status_b, StatusCode::OK);
        assert_eq!(body_b, "bob");
    }

    #[tokio::test]
    async fn empty_configured_token_client_is_dropped_and_never_matches() {
        // #6270: a client constructed with an empty token must not be reachable via an
        // empty presented bearer value (`Authorization: Bearer `), which would otherwise
        // hash-match `blake3::hash(b"")` on both sides.
        let app = app_with_clients(vec![client("bad", ""), client("default", "my-secret")]);
        assert_eq!(send(app, Some("Bearer ")).await.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn whitespace_only_configured_token_client_is_dropped() {
        // #6270 F5: a whitespace-only token is as weak a secret as an empty one — trimmed
        // the same way `resolve_acp_auth_clients` already trims vault-resolved tokens.
        let app = app_with_clients(vec![client("bad", "   "), client("default", "my-secret")]);
        assert_eq!(
            send(app, Some("Bearer    ")).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn multi_client_unknown_token_rejected() {
        let app = app_with_clients(vec![client("alice", "token-a"), client("bob", "token-b")]);
        assert_eq!(
            send(app, Some("Bearer token-c")).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
}
