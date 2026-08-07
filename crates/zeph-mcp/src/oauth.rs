// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! OAuth 2.1 callback listener used by `McpTransport::OAuth` connections.
//!
//! [`await_oauth_callback`] binds a TCP listener before the browser-based authorization
//! flow starts (so the callback port is known when registering the redirect URI), then
//! waits for the browser to redirect back with `?code=...&state=...` query params.

use std::time::Duration;

use futures::StreamExt;
use rmcp::transport::auth::{
    OAuthHttpClient, OAuthHttpClientError, OAuthHttpClientFuture, OAuthHttpRedirectPolicy,
    OAuthHttpRequest,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

use zeph_common::net::resolve_and_validate;

use crate::error::McpError;

/// Timeout applied to an OAuth HTTP request when rmcp does not specify one.
const DEFAULT_OAUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Response body cap mirroring rmcp's internal `ReqwestOAuthHttpClient` limit (not
/// exported by rmcp, so re-declared here to match).
const MAX_OAUTH_HTTP_RESPONSE_BODY_BYTES: usize = 1024 * 1024;

/// Maximum redirect hops [`PinningOAuthHttpClient::execute`] follows for a single
/// `OAuthHttpRedirectPolicy::Follow` request before giving up.
///
/// OAuth flows have no legitimate reason to redirect more than a handful of times;
/// this bounds the manual redirect loop against a malicious or misconfigured server
/// that redirects indefinitely.
const MAX_OAUTH_REDIRECT_HOPS: usize = 10;

/// [`OAuthHttpClient`] that validates and DNS-pins every outbound OAuth HTTP request
/// at the moment it is executed, rather than once up front.
///
/// `AuthorizationManager` may issue requests to hosts that differ from the MCP
/// server's own host — the discovered `token_endpoint`, `authorization_endpoint`,
/// `jwks_uri`, and `registration_endpoint` can legitimately live on a separate OAuth
/// issuer per SEP-985. A single `reqwest::Client` pinned via `resolve_to_addrs` to the
/// MCP server's host (as used for the MCP transport itself) cannot pin those other
/// hosts, so rmcp would fall back to its own independent, unpinned DNS resolution for
/// them — reopening the DNS-rebinding TOCTOU window that pinning closes for the MCP
/// transport. Implementing [`OAuthHttpClient`] lets every OAuth HTTP call — regardless
/// of which host it targets — be resolved, SSRF-validated, and pinned individually,
/// immediately before it is sent. See #6074.
///
/// The same reasoning applies to redirects: a `3xx` response to an
/// `OAuthHttpRedirectPolicy::Follow` request (dynamic client registration is rmcp's
/// only current caller) can point at yet another host. [`Self::build_client`] always
/// disables reqwest's own redirect-following, and [`Self::execute_with_policy`]
/// follows `Follow`-policy redirects manually, re-running the same validate-and-pin
/// step for each hop via [`Self::execute_single`], bounded by
/// [`MAX_OAUTH_REDIRECT_HOPS`]. See #6089.
pub(crate) struct PinningOAuthHttpClient {
    server_id: String,
    trusted: bool,
}

/// Headers that describe a request body — must not be forwarded once
/// [`PinningOAuthHttpClient::next_hop_request`] empties the body on a redirect
/// downgrade, or the next hop would advertise content that isn't there.
fn is_body_describing_header(name: &http::HeaderName) -> bool {
    *name == http::header::CONTENT_LENGTH
        || *name == http::header::CONTENT_TYPE
        || *name == http::header::CONTENT_ENCODING
}

/// Headers that must never be forwarded to a redirect target on a different origin —
/// matches reqwest's own `redirect::remove_sensitive_headers` set. A redirect target
/// passing SSRF validation only proves it isn't a private address; it says nothing
/// about who controls it, so credentials must not follow a cross-origin hop.
fn is_cross_origin_sensitive_header(name: &http::HeaderName) -> bool {
    *name == http::header::AUTHORIZATION
        || *name == http::header::COOKIE
        || *name == http::header::PROXY_AUTHORIZATION
        || *name == http::header::WWW_AUTHENTICATE
}

impl PinningOAuthHttpClient {
    /// Create a client that validates and pins every OAuth HTTP request by host,
    /// unless `trusted` is `true` — matching the bypass semantics already applied to
    /// the MCP transport connection for operator-controlled static config.
    pub(crate) fn new(server_id: impl Into<String>, trusted: bool) -> Self {
        Self {
            server_id: server_id.into(),
            trusted,
        }
    }

    /// Resolve and SSRF-validate `host:port` (skipped when `trusted`), then build a
    /// `reqwest::Client` pinned to the result via `resolve_to_addrs`, honoring the
    /// request's timeout.
    ///
    /// Redirects are always disabled on the returned client (`Policy::none()`),
    /// regardless of the request's [`OAuthHttpRedirectPolicy`] — a client that followed
    /// redirects itself would re-resolve the target host independently and unpinned,
    /// reopening the exact DNS-rebinding TOCTOU this type exists to close. Every hop of
    /// an `OAuthHttpRedirectPolicy::Follow` request is instead executed by a fresh call
    /// to this method (see [`PinningOAuthHttpClient::execute_single`] and the redirect
    /// loop in [`OAuthHttpClient::execute`]), so a redirect target gets the same
    /// independent validate-and-pin treatment as the original request (#6089).
    ///
    /// # Errors
    ///
    /// Returns [`OAuthHttpClientError`] if `host` resolves to a private, loopback, or
    /// link-local address, or if the client cannot be built.
    async fn build_client(
        &self,
        host: &str,
        port: u16,
        timeout: Option<Duration>,
    ) -> Result<reqwest::Client, OAuthHttpClientError> {
        let mut builder = reqwest::Client::builder()
            .timeout(timeout.unwrap_or(DEFAULT_OAUTH_HTTP_TIMEOUT))
            .redirect(reqwest::redirect::Policy::none());

        if self.trusted {
            tracing::debug!(
                server_id = %self.server_id,
                host,
                "oauth http client: trusted connection, skipping SSRF validation"
            );
        } else {
            let addrs = resolve_and_validate(host, port).await.map_err(|e| {
                tracing::warn!(
                    server_id = %self.server_id,
                    host,
                    error = %e,
                    "oauth http client: blocked SSRF-unsafe request target"
                );
                OAuthHttpClientError::from(e.to_string())
            })?;
            tracing::debug!(
                server_id = %self.server_id,
                host,
                addr_count = addrs.len(),
                "oauth http client: pinning request to validated addresses"
            );
            builder = builder.resolve_to_addrs(host, &addrs);
        }

        builder.build().map_err(|e| {
            OAuthHttpClientError::from(format!("failed to build OAuth HTTP client: {e}"))
        })
    }

    /// Validate, DNS-pin, and execute a single HTTP hop — the original request, or one
    /// redirect target reached via [`Self::next_hop_request`]. Called fresh for every
    /// hop so each one gets its own [`Self::build_client`] validate-and-pin cycle.
    async fn execute_single(
        &self,
        request: &http::Request<Vec<u8>>,
        timeout: Option<Duration>,
    ) -> Result<http::Response<Vec<u8>>, OAuthHttpClientError> {
        let uri = request.uri();
        let host = uri
            .host()
            .ok_or_else(|| OAuthHttpClientError::from("OAuth request URI missing host"))?
            .to_owned();
        let port = uri
            .port_u16()
            .unwrap_or(if uri.scheme_str() == Some("http") {
                80
            } else {
                443
            });

        let client = self.build_client(&host, port, timeout).await?;

        let mut builder = client.request(request.method().clone(), uri.to_string());
        for (name, value) in request.headers() {
            builder = builder.header(name, value);
        }
        builder = builder.body(request.body().clone());

        let response = builder
            .send()
            .await
            .map_err(|e| OAuthHttpClientError::from(e.to_string()))?;

        let mut resp_builder = http::Response::builder()
            .status(response.status())
            .version(response.version());
        for (name, value) in response.headers() {
            resp_builder = resp_builder.header(name, value);
        }

        let mut body = Vec::new();
        let mut body_stream = response.bytes_stream();
        while let Some(chunk) = body_stream.next().await {
            let chunk = chunk.map_err(|e| OAuthHttpClientError::from(e.to_string()))?;
            if chunk.len() > MAX_OAUTH_HTTP_RESPONSE_BODY_BYTES.saturating_sub(body.len()) {
                return Err(OAuthHttpClientError::from(format!(
                    "OAuth HTTP response body exceeds {MAX_OAUTH_HTTP_RESPONSE_BODY_BYTES} bytes"
                )));
            }
            body.extend_from_slice(&chunk);
        }

        resp_builder
            .body(body)
            .map_err(|e| OAuthHttpClientError::from(e.to_string()))
    }

    /// Build the request for the next redirect hop from the previous request/response
    /// pair.
    ///
    /// Mirrors reqwest's own default redirect semantics (the behavior a `Follow`-policy
    /// request had before redirects were disabled at the client level): `303` always
    /// downgrades to `GET` with no body; `301`/`302` downgrade a `POST` to `GET`; every
    /// other redirect status (notably `307`/`308`) preserves the original method and
    /// body. Content-describing headers (`Content-Length`/`Content-Type`/
    /// `Content-Encoding`) are dropped when the body is emptied, and sensitive headers
    /// (`Authorization`/`Cookie`/`Proxy-Authorization`/`WWW-Authenticate`) are dropped
    /// whenever the redirect target's origin (scheme, host, or port) differs from the
    /// original request's — a validated, SSRF-safe *public* redirect target can still be
    /// attacker-controlled, so credentials must never follow it across origins.
    fn next_hop_request(
        prev: &http::Request<Vec<u8>>,
        response: &http::Response<Vec<u8>>,
    ) -> Result<http::Request<Vec<u8>>, OAuthHttpClientError> {
        let location = response
            .headers()
            .get(http::header::LOCATION)
            .ok_or_else(|| OAuthHttpClientError::from("redirect response missing Location header"))?
            .to_str()
            .map_err(|e| OAuthHttpClientError::from(format!("invalid Location header: {e}")))?;

        let base = Url::parse(&prev.uri().to_string())
            .map_err(|e| OAuthHttpClientError::from(format!("invalid request URI: {e}")))?;
        let next_url = base.join(location).map_err(|e| {
            OAuthHttpClientError::from(format!("invalid redirect target '{location}': {e}"))
        })?;

        let downgrade_to_get = response.status() == http::StatusCode::SEE_OTHER
            || ((response.status() == http::StatusCode::MOVED_PERMANENTLY
                || response.status() == http::StatusCode::FOUND)
                && prev.method() == http::Method::POST);
        let (method, body) = if downgrade_to_get {
            (http::Method::GET, Vec::new())
        } else {
            (prev.method().clone(), prev.body().clone())
        };

        let cross_origin = (base.scheme(), base.host_str(), base.port_or_known_default())
            != (
                next_url.scheme(),
                next_url.host_str(),
                next_url.port_or_known_default(),
            );
        let body_dropped = body.is_empty();

        let mut builder = http::Request::builder()
            .method(method)
            .uri(next_url.as_str());
        for (name, value) in prev.headers() {
            if body_dropped && is_body_describing_header(name) {
                continue;
            }
            if cross_origin && is_cross_origin_sensitive_header(name) {
                continue;
            }
            builder = builder.header(name, value);
        }
        builder
            .body(body)
            .map_err(|e| OAuthHttpClientError::from(e.to_string()))
    }

    /// Redirect-following loop shared by [`OAuthHttpClient::execute`] and tests.
    ///
    /// Decoupled from rmcp's `OAuthHttpRequest` wrapper (which has no public
    /// constructor, so it cannot be built outside rmcp) so the redirect-pinning
    /// behavior can be exercised directly with plain `http::Request` values.
    async fn execute_with_policy(
        &self,
        mut request: http::Request<Vec<u8>>,
        redirect_policy: OAuthHttpRedirectPolicy,
        timeout: Option<Duration>,
    ) -> Result<http::Response<Vec<u8>>, OAuthHttpClientError> {
        let mut hops = 0usize;
        loop {
            let response = self.execute_single(&request, timeout).await?;
            if redirect_policy != OAuthHttpRedirectPolicy::Follow
                || !response.status().is_redirection()
            {
                return Ok(response);
            }

            hops += 1;
            if hops > MAX_OAUTH_REDIRECT_HOPS {
                return Err(OAuthHttpClientError::from(format!(
                    "OAuth request exceeded max redirect hops ({MAX_OAUTH_REDIRECT_HOPS})"
                )));
            }
            request = Self::next_hop_request(&request, &response)?;
        }
    }
}

impl OAuthHttpClient for PinningOAuthHttpClient {
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move {
            let OAuthHttpRequest {
                request,
                redirect_policy,
                timeout,
                ..
            } = request;
            self.execute_with_policy(request, redirect_policy, timeout)
                .await
        })
    }
}

/// Build an [`OAuthHttpClient`] that per-request validates and pins the target host,
/// for use with `OAuthState::new_with_oauth_http_client` / `AuthorizationManager::
/// new_with_oauth_http_client`.
pub(crate) fn pinning_oauth_http_client(
    server_id: &str,
    trusted: bool,
) -> std::sync::Arc<dyn OAuthHttpClient> {
    std::sync::Arc::new(PinningOAuthHttpClient::new(server_id, trusted))
}

/// Await an OAuth callback on the given pre-bound listener.
///
/// Reads one HTTP GET request, extracts `?code=...&state=...` query parameters,
/// writes a minimal success response, and returns `(code, state)`.
///
/// The listener must already be bound (so the port is known before client registration).
///
/// # Errors
///
/// Returns `McpError::OAuthCallbackTimeout` if no callback arrives within `timeout`,
/// or `McpError::OAuthError` on parse failures.
#[cfg_attr(
    feature = "profiling",
    tracing::instrument(
        name = "mcp.oauth.await_oauth_callback",
        skip(listener),
        fields(server_id)
    )
)]
pub async fn await_oauth_callback(
    listener: tokio::net::TcpListener,
    timeout: Duration,
    server_id: &str,
) -> Result<(String, String), McpError> {
    let accept_fut = async {
        let (mut stream, _) = listener.accept().await.map_err(|e| McpError::OAuthError {
            server_id: server_id.to_owned(),
            message: format!("callback server accept failed: {e}"),
        })?;

        // Read in a loop until the HTTP header terminator (\r\n\r\n) is found or the
        // buffer reaches the cap. A single read() may return a partial TCP segment.
        let mut buf = Vec::with_capacity(4096);
        let cap: usize = 8192;
        loop {
            let mut chunk = [0u8; 512];
            let n = stream
                .read(&mut chunk)
                .await
                .map_err(|e| McpError::OAuthError {
                    server_id: server_id.to_owned(),
                    message: format!("callback read failed: {e}"),
                })?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.array_windows::<4>().any(|w| w == b"\r\n\r\n") || buf.len() >= cap {
                break;
            }
        }
        let request = String::from_utf8_lossy(&buf);

        // Extract request line: "GET /callback?code=...&state=... HTTP/1.1"
        let first_line = request.lines().next().unwrap_or_default();
        let path = first_line.split_whitespace().nth(1).unwrap_or_default();

        let query = path.split_once('?').map(|(_, q)| q).unwrap_or_default();

        let (code, state) = parse_callback_params(query, server_id)?;

        let response = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nAuthorization successful. You can close this tab.";
        let _ = stream.write_all(response.as_bytes()).await;

        Ok::<(String, String), McpError>((code, state))
    };

    tokio::time::timeout(timeout, accept_fut)
        .await
        .map_err(|_| McpError::OAuthCallbackTimeout {
            server_id: server_id.to_owned(),
            timeout_secs: timeout.as_secs(),
        })?
}

fn parse_callback_params(query: &str, server_id: &str) -> Result<(String, String), McpError> {
    let mut code = None;
    let mut state = None;

    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let v = urlencoding_decode(v);
            match k {
                "code" => code = Some(v),
                "state" => state = Some(v),
                _ => {}
            }
        }
    }

    let code = code.ok_or_else(|| McpError::OAuthError {
        server_id: server_id.to_owned(),
        message: "OAuth callback missing 'code' parameter".into(),
    })?;
    let state = state.ok_or_else(|| McpError::OAuthError {
        server_id: server_id.to_owned(),
        message: "OAuth callback missing 'state' parameter".into(),
    })?;

    Ok((code, state))
}

/// Minimal percent-decode for OAuth callback params (replace `%XX` and `+`).
fn urlencoding_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(char::from(h * 16 + l));
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            out.push(' ');
            i += 1;
            continue;
        }
        out.push(char::from(bytes[i]));
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Fail-fast pre-check: validate that OAuth metadata endpoints don't resolve to
/// private IPs before starting the (user-interactive) authorization flow.
///
/// Called after `resolve_metadata()`, before using any of the discovered URLs.
///
/// This is no longer the security boundary for SSRF on these endpoints — every
/// actual OAuth HTTP request (token exchange, refresh, dynamic client registration)
/// is independently resolved, validated, and DNS-pinned at execution time by
/// `PinningOAuthHttpClient`, which closes the cross-origin TOCTOU that a
/// validate-then-discard pre-check like this one cannot (see #6074). This function is
/// kept purely so a misconfigured/malicious endpoint is rejected before opening a
/// browser tab for the user, rather than after they complete the authorization dance.
///
/// # Errors
///
/// Returns `McpError::OAuthError` if any endpoint resolves to a private/reserved IP.
#[cfg_attr(
    feature = "profiling",
    tracing::instrument(
        name = "mcp.oauth.validate_oauth_metadata_urls",
        skip(metadata),
        fields(server_id)
    )
)]
pub async fn validate_oauth_metadata_urls(
    server_id: &str,
    metadata: &rmcp::transport::auth::AuthorizationMetadata,
) -> Result<(), McpError> {
    use crate::client::validate_url_ssrf;

    validate_url_ssrf(&metadata.token_endpoint)
        .await
        .map_err(|_| McpError::OAuthError {
            server_id: server_id.to_owned(),
            message: format!(
                "SSRF: token_endpoint '{}' resolves to private IP",
                metadata.token_endpoint
            ),
        })?;

    if let Some(ref reg_url) = metadata.registration_endpoint {
        validate_url_ssrf(reg_url)
            .await
            .map_err(|_| McpError::OAuthError {
                server_id: server_id.to_owned(),
                message: format!("SSRF: registration_endpoint '{reg_url}' resolves to private IP"),
            })?;
    }

    validate_url_ssrf(&metadata.authorization_endpoint)
        .await
        .map_err(|_| McpError::OAuthError {
            server_id: server_id.to_owned(),
            message: format!(
                "SSRF: authorization_endpoint '{}' resolves to private IP",
                metadata.authorization_endpoint
            ),
        })?;

    if let Some(ref jwks) = metadata.jwks_uri {
        validate_url_ssrf(jwks)
            .await
            .map_err(|_| McpError::OAuthError {
                server_id: server_id.to_owned(),
                message: format!("SSRF: jwks_uri '{jwks}' resolves to private IP"),
            })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[test]
    fn urlencoding_decode_basic() {
        assert_eq!(urlencoding_decode("hello+world"), "hello world");
        assert_eq!(urlencoding_decode("foo%20bar"), "foo bar");
        assert_eq!(urlencoding_decode("abc%2F"), "abc/");
    }

    #[test]
    fn parse_callback_params_ok() {
        let (code, state) = parse_callback_params("code=abc123&state=xyz", "srv").unwrap();
        assert_eq!(code, "abc123");
        assert_eq!(state, "xyz");
    }

    #[test]
    fn parse_callback_params_missing_code() {
        let err = parse_callback_params("state=xyz", "srv").unwrap_err();
        assert_matches!(err, McpError::OAuthError { .. });
    }

    #[test]
    fn parse_callback_params_missing_state() {
        let err = parse_callback_params("code=abc", "srv").unwrap_err();
        assert_matches!(err, McpError::OAuthError { .. });
    }

    #[test]
    fn oauth_error_variants_display() {
        let err = McpError::OAuthError {
            server_id: "todoist".into(),
            message: "metadata discovery failed".into(),
        };
        assert!(err.to_string().contains("todoist"));
        assert!(err.to_string().contains("metadata discovery failed"));

        let timeout = McpError::OAuthCallbackTimeout {
            server_id: "todoist".into(),
            timeout_secs: 300,
        };
        assert!(timeout.to_string().contains("300"));
    }

    // TC-07: validate_oauth_metadata_urls blocks private IPs on all three endpoints.
    // Uses 8.8.8.8 as a "public" IP literal (no DNS) to avoid network dependency in passing fields.
    #[tokio::test]
    async fn validate_oauth_metadata_urls_blocks_private_token_endpoint() {
        let mut metadata = rmcp::transport::auth::AuthorizationMetadata::default();
        // token_endpoint is private — must be rejected
        metadata.token_endpoint = "http://10.0.0.1/token".into();
        // other endpoints use a literal public IP so DNS is not required
        metadata.authorization_endpoint = "http://8.8.8.8/auth".into();
        let err = validate_oauth_metadata_urls("srv", &metadata)
            .await
            .unwrap_err();
        assert_matches!(err, McpError::OAuthError { .. });
        assert!(err.to_string().contains("token_endpoint"));
    }

    #[tokio::test]
    async fn validate_oauth_metadata_urls_blocks_private_authorization_endpoint() {
        let mut metadata = rmcp::transport::auth::AuthorizationMetadata::default();
        // token_endpoint uses literal public IP so it passes
        metadata.token_endpoint = "http://8.8.8.8/token".into();
        // authorization_endpoint is private — must be rejected
        metadata.authorization_endpoint = "http://192.168.1.1/auth".into();
        let err = validate_oauth_metadata_urls("srv", &metadata)
            .await
            .unwrap_err();
        assert_matches!(err, McpError::OAuthError { .. });
        assert!(err.to_string().contains("authorization_endpoint"));
    }

    #[tokio::test]
    async fn validate_oauth_metadata_urls_blocks_private_jwks_uri() {
        let mut metadata = rmcp::transport::auth::AuthorizationMetadata::default();
        // token_endpoint and authorization_endpoint use literal public IPs
        metadata.token_endpoint = "http://8.8.8.8/token".into();
        metadata.authorization_endpoint = "http://8.8.8.8/auth".into();
        // jwks_uri is private — must be rejected
        metadata.jwks_uri = Some("http://127.0.0.1:9000/jwks".into());
        let err = validate_oauth_metadata_urls("srv", &metadata)
            .await
            .unwrap_err();
        assert_matches!(err, McpError::OAuthError { .. });
        assert!(err.to_string().contains("jwks_uri"));
    }

    // #6074: PinningOAuthHttpClient validates and pins each OAuth HTTP request by its
    // own target host at execution time, not just once up front against the MCP
    // server's host. `build_client` is the per-request validate+pin step `execute()`
    // calls for every outbound request — exercising it directly proves the request-time
    // enforcement without needing rmcp's private `OAuthHttpRequest` constructor.

    #[tokio::test]
    async fn pinning_oauth_http_client_blocks_private_cross_origin_host() {
        // Simulates a discovered `token_endpoint` on a different (private) host than
        // the already-validated MCP server — exactly the SEP-985 cross-origin case a
        // single MCP-transport-pinned client cannot catch.
        let client = PinningOAuthHttpClient::new("srv", false);
        let err = client.build_client("10.0.0.1", 80, None).await.unwrap_err();
        assert!(err.to_string().contains("SSRF protection"));
    }

    #[tokio::test]
    async fn pinning_oauth_http_client_blocks_private_loopback_host() {
        let client = PinningOAuthHttpClient::new("srv", false);
        let err = client
            .build_client("127.0.0.1", 443, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("SSRF protection"));
    }

    #[tokio::test]
    async fn pinning_oauth_http_client_allows_public_ip_literal() {
        let client = PinningOAuthHttpClient::new("srv", false);
        client
            .build_client("8.8.8.8", 443, None)
            .await
            .expect("public IP literal must not be blocked");
    }

    #[tokio::test]
    async fn pinning_oauth_http_client_trusted_mode_skips_validation() {
        // Trusted connections bypass SSRF validation entirely, matching the same
        // bypass already applied to the MCP transport's own pinning for
        // operator-controlled static config.
        let client = PinningOAuthHttpClient::new("srv", true);
        client
            .build_client("127.0.0.1", 443, None)
            .await
            .expect("trusted client must skip SSRF validation");
    }

    // #6089: a `Follow`-policy request that hits a redirect must have the redirect
    // target independently validated and DNS-pinned too, not just the original host.
    // `build_client` now always disables reqwest's own redirect-following
    // (`Policy::none()` unconditionally), so these tests double as a regression check:
    // if the manual redirect loop below were missing or broken, a `Follow` request
    // would come back with the raw, unfollowed 3xx instead of the final response.

    // --- next_hop_request (pure logic, no network) ---

    #[test]
    fn next_hop_request_resolves_relative_location_against_request_uri() {
        let prev = http::Request::builder()
            .method("GET")
            .uri("http://origin.example/start")
            .body(Vec::new())
            .unwrap();
        let response = http::Response::builder()
            .status(302)
            .header("Location", "/next")
            .body(Vec::new())
            .unwrap();

        let next = PinningOAuthHttpClient::next_hop_request(&prev, &response).unwrap();
        assert_eq!(next.uri(), "http://origin.example/next");
        assert_eq!(next.method(), http::Method::GET);
    }

    #[test]
    fn next_hop_request_downgrades_post_to_get_on_302() {
        let prev = http::Request::builder()
            .method("POST")
            .uri("http://origin.example/register")
            .body(b"{}".to_vec())
            .unwrap();
        let response = http::Response::builder()
            .status(302)
            .header("Location", "http://other.example/next")
            .body(Vec::new())
            .unwrap();

        let next = PinningOAuthHttpClient::next_hop_request(&prev, &response).unwrap();
        assert_eq!(next.method(), http::Method::GET);
        assert!(next.body().is_empty());
        assert_eq!(next.uri(), "http://other.example/next");
    }

    #[test]
    fn next_hop_request_preserves_method_and_body_on_307() {
        let prev = http::Request::builder()
            .method("POST")
            .uri("http://origin.example/register")
            .body(br#"{"x":1}"#.to_vec())
            .unwrap();
        let response = http::Response::builder()
            .status(307)
            .header("Location", "http://other.example/next")
            .body(Vec::new())
            .unwrap();

        let next = PinningOAuthHttpClient::next_hop_request(&prev, &response).unwrap();
        assert_eq!(next.method(), http::Method::POST);
        assert_eq!(next.body(), br#"{"x":1}"#);
    }

    #[test]
    fn next_hop_request_rejects_missing_location_header() {
        let prev = http::Request::builder()
            .method("GET")
            .uri("http://origin.example/start")
            .body(Vec::new())
            .unwrap();
        let response = http::Response::builder()
            .status(302)
            .body(Vec::new())
            .unwrap();

        let err = PinningOAuthHttpClient::next_hop_request(&prev, &response).unwrap_err();
        assert!(err.to_string().contains("Location"));
    }

    #[test]
    fn next_hop_request_strips_sensitive_headers_on_cross_origin_redirect() {
        let prev = http::Request::builder()
            .method("GET")
            .uri("http://origin.example/start")
            .header(http::header::AUTHORIZATION, "Bearer secret")
            .header(http::header::COOKIE, "session=abc")
            .body(Vec::new())
            .unwrap();
        let response = http::Response::builder()
            .status(302)
            .header("Location", "http://other.example/next")
            .body(Vec::new())
            .unwrap();

        let next = PinningOAuthHttpClient::next_hop_request(&prev, &response).unwrap();
        assert!(next.headers().get(http::header::AUTHORIZATION).is_none());
        assert!(next.headers().get(http::header::COOKIE).is_none());
    }

    #[test]
    fn next_hop_request_preserves_sensitive_headers_on_same_origin_redirect() {
        let prev = http::Request::builder()
            .method("GET")
            .uri("http://origin.example/start")
            .header(http::header::AUTHORIZATION, "Bearer secret")
            .body(Vec::new())
            .unwrap();
        let response = http::Response::builder()
            .status(307)
            .header("Location", "/next")
            .body(Vec::new())
            .unwrap();

        let next = PinningOAuthHttpClient::next_hop_request(&prev, &response).unwrap();
        assert_eq!(
            next.headers().get(http::header::AUTHORIZATION).unwrap(),
            "Bearer secret"
        );
    }

    #[test]
    fn next_hop_request_strips_sensitive_headers_on_scheme_change_same_host() {
        // Same host, but http -> https is still a different origin per the fetch spec
        // and reqwest's own redirect handling — must be treated as cross-origin.
        let prev = http::Request::builder()
            .method("GET")
            .uri("http://origin.example/start")
            .header(http::header::AUTHORIZATION, "Bearer secret")
            .body(Vec::new())
            .unwrap();
        let response = http::Response::builder()
            .status(302)
            .header("Location", "https://origin.example/next")
            .body(Vec::new())
            .unwrap();

        let next = PinningOAuthHttpClient::next_hop_request(&prev, &response).unwrap();
        assert!(next.headers().get(http::header::AUTHORIZATION).is_none());
    }

    #[test]
    fn next_hop_request_drops_content_headers_when_body_downgraded() {
        let prev = http::Request::builder()
            .method("POST")
            .uri("http://origin.example/register")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(http::header::CONTENT_ENCODING, "gzip")
            .body(b"{}".to_vec())
            .unwrap();
        let response = http::Response::builder()
            .status(302)
            .header("Location", "http://other.example/next")
            .body(Vec::new())
            .unwrap();

        let next = PinningOAuthHttpClient::next_hop_request(&prev, &response).unwrap();
        assert!(next.headers().get(http::header::CONTENT_TYPE).is_none());
        assert!(next.headers().get(http::header::CONTENT_ENCODING).is_none());
        assert!(next.body().is_empty());
    }

    // --- execute_with_policy / execute_single (network, wiremock) ---

    /// `Stop` policy behavior is unchanged by this fix: the raw redirect response is
    /// returned unfollowed. Metadata discovery, token exchange, and token refresh all
    /// use `Stop`, so this is the regression check for those existing flows.
    #[tokio::test]
    async fn pinning_oauth_http_client_stop_policy_returns_redirect_unfollowed() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(302)
                    .insert_header("Location", "http://redirect-target.invalid/next"),
            )
            .mount(&server)
            .await;

        let client = PinningOAuthHttpClient::new("srv", true);
        let request = http::Request::builder()
            .method("GET")
            .uri(format!("{}/start", server.uri()))
            .body(Vec::new())
            .unwrap();

        let response = client
            .execute_with_policy(request, OAuthHttpRedirectPolicy::Stop, None)
            .await
            .unwrap();
        assert_eq!(response.status(), http::StatusCode::FOUND);
    }

    /// A `Follow`-policy redirect to a different host is actually followed by the
    /// manual loop — not by reqwest's own redirect-following, which `build_client` now
    /// disables unconditionally. Reaching the second, distinct mock server proves the
    /// loop re-invokes `build_client`/`execute_single` for the new target rather than
    /// silently returning the unfollowed 3xx (which is what would happen if the manual
    /// loop were missing).
    ///
    /// This uses `trusted: true`, so it does *not* by itself prove the redirect target
    /// gets SSRF-revalidated — both hosts here are loopback, which strict validation
    /// would reject regardless of whether the redirect target is handled correctly, so
    /// there is no way to drive this end-to-end under `trusted: false` with a local
    /// mock server. That the identical `execute_single` validate-and-pin step used for
    /// the initial hop also runs for every redirect hop is proven separately by
    /// `pinning_oauth_http_client_execute_single_blocks_ssrf_unsafe_target`; composing
    /// the two properties (this test's "different host is reached" +
    /// that test's "this exact function rejects an unsafe target") establishes the
    /// #6089 guarantee.
    #[tokio::test]
    async fn pinning_oauth_http_client_follows_redirect_to_different_host() {
        let target = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("final"))
            .mount(&target)
            .await;

        let origin = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/next", target.uri())),
            )
            .mount(&origin)
            .await;

        let client = PinningOAuthHttpClient::new("srv", true);
        let request = http::Request::builder()
            .method("GET")
            .uri(format!("{}/start", origin.uri()))
            .body(Vec::new())
            .unwrap();

        let response = client
            .execute_with_policy(request, OAuthHttpRedirectPolicy::Follow, None)
            .await
            .unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(response.body(), b"final");
    }

    /// End-to-end version of the 302 POST->GET downgrade for dynamic client
    /// registration (the one current `Follow`-policy caller): if the redirected
    /// request incorrectly kept POST, the target mock (which only matches GET) would
    /// not fire and the response would be wiremock's default 404 instead of 200.
    #[tokio::test]
    async fn pinning_oauth_http_client_post_redirect_downgrades_to_get() {
        let target = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("got-get"))
            .mount(&target)
            .await;

        let origin = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/next", target.uri())),
            )
            .mount(&origin)
            .await;

        let client = PinningOAuthHttpClient::new("srv", true);
        let request = http::Request::builder()
            .method("POST")
            .uri(format!("{}/register", origin.uri()))
            .body(b"{}".to_vec())
            .unwrap();

        let response = client
            .execute_with_policy(request, OAuthHttpRedirectPolicy::Follow, None)
            .await
            .unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(response.body(), b"got-get");
    }

    /// A server that redirects forever must not hang the caller — bounded by
    /// `MAX_OAUTH_REDIRECT_HOPS`.
    #[tokio::test]
    async fn pinning_oauth_http_client_bounds_redirect_hops() {
        let server = wiremock::MockServer::start().await;
        let loop_url = format!("{}/loop", server.uri());
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(302).insert_header("Location", loop_url.as_str()),
            )
            .mount(&server)
            .await;

        let client = PinningOAuthHttpClient::new("srv", true);
        let request = http::Request::builder()
            .method("GET")
            .uri(loop_url)
            .body(Vec::new())
            .unwrap();

        let err = client
            .execute_with_policy(request, OAuthHttpRedirectPolicy::Follow, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("max redirect hops"));
    }

    /// `execute_single` is the exact function the redirect loop calls for *every* hop,
    /// including redirect targets (see `execute_with_policy`). Proving it rejects a
    /// private-IP target with `trusted: false` shows a malicious redirect to an
    /// internal address would be rejected exactly like a private initial host would —
    /// the core #6089 security property — without needing an actually-reachable public
    /// first hop to drive a full end-to-end redirect chain under strict SSRF
    /// validation.
    #[tokio::test]
    async fn pinning_oauth_http_client_execute_single_blocks_ssrf_unsafe_target() {
        let client = PinningOAuthHttpClient::new("srv", false);
        let request = http::Request::builder()
            .method("GET")
            .uri("http://127.0.0.1:9/register")
            .body(Vec::new())
            .unwrap();

        let err = client.execute_single(&request, None).await.unwrap_err();
        assert!(err.to_string().contains("SSRF protection"));
    }
}
