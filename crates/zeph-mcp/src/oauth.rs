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

use zeph_common::net::resolve_and_validate;

use crate::error::McpError;

/// Timeout applied to an OAuth HTTP request when rmcp does not specify one.
const DEFAULT_OAUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Response body cap mirroring rmcp's internal `ReqwestOAuthHttpClient` limit (not
/// exported by rmcp, so re-declared here to match).
const MAX_OAUTH_HTTP_RESPONSE_BODY_BYTES: usize = 1024 * 1024;

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
pub(crate) struct PinningOAuthHttpClient {
    server_id: String,
    trusted: bool,
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
    /// request's redirect policy and timeout.
    ///
    /// # Errors
    ///
    /// Returns [`OAuthHttpClientError`] if `host` resolves to a private, loopback, or
    /// link-local address, or if the client cannot be built.
    // TODO(critic): `OAuthHttpRedirectPolicy::Follow` requests (e.g. dynamic client
    // registration) only pin the originally-resolved host; if the response is itself a
    // redirect to a *different* host, reqwest re-resolves that hop unpinned. Tracked in
    // #6089.
    async fn build_client(
        &self,
        host: &str,
        port: u16,
        redirect_policy: OAuthHttpRedirectPolicy,
        timeout: Option<Duration>,
    ) -> Result<reqwest::Client, OAuthHttpClientError> {
        let mut builder =
            reqwest::Client::builder().timeout(timeout.unwrap_or(DEFAULT_OAUTH_HTTP_TIMEOUT));
        if redirect_policy == OAuthHttpRedirectPolicy::Stop {
            builder = builder.redirect(reqwest::redirect::Policy::none());
        }

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
                OAuthHttpClientError::new(e.to_string())
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
            OAuthHttpClientError::new(format!("failed to build OAuth HTTP client: {e}"))
        })
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

            let uri = request.uri();
            let host = uri
                .host()
                .ok_or_else(|| OAuthHttpClientError::new("OAuth request URI missing host"))?
                .to_owned();
            let port = uri
                .port_u16()
                .unwrap_or(if uri.scheme_str() == Some("http") {
                    80
                } else {
                    443
                });

            let client = self
                .build_client(&host, port, redirect_policy, timeout)
                .await?;

            let reqwest_request = reqwest::Request::try_from(request)
                .map_err(|e| OAuthHttpClientError::new(e.to_string()))?;

            let response = client
                .execute(reqwest_request)
                .await
                .map_err(|e| OAuthHttpClientError::new(e.to_string()))?;

            let mut builder = http::Response::builder()
                .status(response.status())
                .version(response.version());
            for (name, value) in response.headers() {
                builder = builder.header(name, value);
            }

            let mut body = Vec::new();
            let mut body_stream = response.bytes_stream();
            while let Some(chunk) = body_stream.next().await {
                let chunk = chunk.map_err(|e| OAuthHttpClientError::new(e.to_string()))?;
                if chunk.len() > MAX_OAUTH_HTTP_RESPONSE_BODY_BYTES.saturating_sub(body.len()) {
                    return Err(OAuthHttpClientError::new(format!(
                        "OAuth HTTP response body exceeds {MAX_OAUTH_HTTP_RESPONSE_BODY_BYTES} bytes"
                    )));
                }
                body.extend_from_slice(&chunk);
            }

            builder
                .body(body)
                .map_err(|e| OAuthHttpClientError::new(e.to_string()))
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
/// Called after `discover_metadata()`, before using any of the discovered URLs.
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
        let err = client
            .build_client("10.0.0.1", 80, OAuthHttpRedirectPolicy::Stop, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("SSRF protection"));
    }

    #[tokio::test]
    async fn pinning_oauth_http_client_blocks_private_loopback_host() {
        let client = PinningOAuthHttpClient::new("srv", false);
        let err = client
            .build_client("127.0.0.1", 443, OAuthHttpRedirectPolicy::Follow, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("SSRF protection"));
    }

    #[tokio::test]
    async fn pinning_oauth_http_client_allows_public_ip_literal() {
        let client = PinningOAuthHttpClient::new("srv", false);
        client
            .build_client("8.8.8.8", 443, OAuthHttpRedirectPolicy::Stop, None)
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
            .build_client("127.0.0.1", 443, OAuthHttpRedirectPolicy::Stop, None)
            .await
            .expect("trusted client must skip SSRF validation");
    }
}
