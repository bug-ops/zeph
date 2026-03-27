// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::McpError;

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
            if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= cap {
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

/// Validate that OAuth metadata endpoints don't resolve to private IPs.
///
/// Called after `discover_metadata()`, before using any of the discovered URLs.
///
/// # Errors
///
/// Returns `McpError::OAuthError` if any endpoint resolves to a private/reserved IP.
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
        assert!(matches!(err, McpError::OAuthError { .. }));
    }

    #[test]
    fn parse_callback_params_missing_state() {
        let err = parse_callback_params("code=abc", "srv").unwrap_err();
        assert!(matches!(err, McpError::OAuthError { .. }));
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
        assert!(matches!(err, McpError::OAuthError { .. }));
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
        assert!(matches!(err, McpError::OAuthError { .. }));
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
        assert!(matches!(err, McpError::OAuthError { .. }));
        assert!(err.to_string().contains("jwks_uri"));
    }
}
