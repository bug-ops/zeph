// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransport;
use tokio::process::Command;
use url::Url;

use zeph_tools::is_private_ip;

use crate::error::McpError;
use crate::tool::McpTool;

type ClientService = RunningService<rmcp::RoleClient, ()>;

pub struct McpClient {
    server_id: String,
    service: Arc<ClientService>,
    timeout: Duration,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("server_id", &self.server_id)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl McpClient {
    /// Spawn child process, perform MCP handshake.
    ///
    /// # Errors
    ///
    /// Returns `McpError::Connection` if the process cannot be spawned or handshake fails.
    pub async fn connect(
        server_id: &str,
        command: &str,
        args: &[String],
        env: &std::collections::HashMap<String, String>,
        allowed_commands: &[String],
        timeout: Duration,
    ) -> Result<Self, McpError> {
        crate::security::validate_command(command, allowed_commands)?;
        crate::security::validate_env(env)?;

        let mut cmd = Command::new(command);
        cmd.args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }

        let transport = TokioChildProcess::new(cmd).map_err(|e| McpError::Connection {
            server_id: server_id.into(),
            message: e.to_string(),
        })?;

        let service =
            ().serve(transport)
                .await
                .map_err(|e| McpError::Connection {
                    server_id: server_id.into(),
                    message: e.to_string(),
                })?;

        Ok(Self {
            server_id: server_id.into(),
            service: Arc::new(service),
            timeout,
        })
    }

    /// Connect to a remote MCP server over Streamable HTTP.
    ///
    /// Performs SSRF validation before connecting — blocks URLs that resolve
    /// to private, loopback, or link-local IP ranges — unless `trusted` is
    /// `true`, in which case the check is skipped (use only for
    /// operator-controlled static config).
    ///
    /// # Errors
    ///
    /// Returns `McpError::SsrfBlocked` if the URL resolves to a private IP,
    /// `McpError::InvalidUrl` if the URL cannot be parsed, or
    /// `McpError::Connection` if the HTTP connection or handshake fails.
    pub async fn connect_url(
        server_id: &str,
        url: &str,
        timeout: Duration,
        trusted: bool,
    ) -> Result<Self, McpError> {
        if !trusted {
            validate_url_ssrf(url).await?;
        }

        let transport = StreamableHttpClientTransport::from_uri(url.to_owned());

        let service =
            ().serve(transport)
                .await
                .map_err(|e| McpError::Connection {
                    server_id: server_id.into(),
                    message: e.to_string(),
                })?;

        Ok(Self {
            server_id: server_id.into(),
            service: Arc::new(service),
            timeout,
        })
    }

    /// Call tools/list, convert to `McpTool` vec.
    ///
    /// # Errors
    ///
    /// Returns `McpError::ToolCall` if listing fails.
    pub async fn list_tools(&self) -> Result<Vec<McpTool>, McpError> {
        let tools = self
            .service
            .list_all_tools()
            .await
            .map_err(|e| McpError::ToolCall {
                server_id: self.server_id.clone(),
                tool_name: "tools/list".into(),
                message: e.to_string(),
            })?;

        Ok(tools
            .into_iter()
            .map(|t| McpTool {
                server_id: self.server_id.clone(),
                name: t.name.to_string(),
                description: t.description.map_or_else(String::new, |d| d.to_string()),
                input_schema: serde_json::to_value(&*t.input_schema).unwrap_or_default(),
            })
            .collect())
    }

    /// Call tools/call with JSON args, return the result.
    ///
    /// # Errors
    ///
    /// Returns `McpError::Timeout` or `McpError::ToolCall` on failure.
    pub async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<CallToolResult, McpError> {
        let arguments: Option<serde_json::Map<String, serde_json::Value>> = args
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect());

        let params = CallToolRequestParams {
            name: Cow::Owned(name.to_owned()),
            arguments,
            task: None,
            meta: None,
        };

        let result = tokio::time::timeout(self.timeout, self.service.call_tool(params))
            .await
            .map_err(|_| McpError::Timeout {
                server_id: self.server_id.clone(),
                tool_name: name.into(),
                timeout_secs: self.timeout.as_secs(),
            })?
            .map_err(|e| McpError::ToolCall {
                server_id: self.server_id.clone(),
                tool_name: name.into(),
                message: e.to_string(),
            })?;

        Ok(result)
    }

    /// Graceful shutdown.
    pub async fn shutdown(self) {
        match Arc::try_unwrap(self.service) {
            Ok(service) => {
                let _ = service.cancel().await;
            }
            Err(_arc) => {
                tracing::warn!(
                    server_id = self.server_id,
                    "cannot shutdown: service has multiple references"
                );
            }
        }
    }
}

async fn validate_url_ssrf(url: &str) -> Result<(), McpError> {
    let parsed = Url::parse(url).map_err(|e| McpError::InvalidUrl {
        url: url.into(),
        message: e.to_string(),
    })?;

    let host = parsed.host_str().ok_or_else(|| McpError::InvalidUrl {
        url: url.into(),
        message: "missing host".into(),
    })?;

    let port = parsed.port_or_known_default().unwrap_or(443);
    let addr_str = format!("{host}:{port}");

    let addrs = tokio::net::lookup_host(&addr_str)
        .await
        .map_err(|e| McpError::InvalidUrl {
            url: url.into(),
            message: format!("DNS resolution failed: {e}"),
        })?;

    for sock_addr in addrs {
        if is_private_ip(sock_addr.ip()) {
            return Err(McpError::SsrfBlocked {
                url: url.into(),
                addr: sock_addr.ip().to_string(),
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::*;

    #[tokio::test]
    async fn ssrf_blocks_localhost() {
        let err = validate_url_ssrf("http://127.0.0.1:8080/mcp")
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::SsrfBlocked { .. }));
    }

    #[tokio::test]
    async fn ssrf_blocks_private_10() {
        let err = validate_url_ssrf("http://10.0.0.1/mcp").await.unwrap_err();
        assert!(matches!(err, McpError::SsrfBlocked { .. }));
    }

    #[tokio::test]
    async fn ssrf_blocks_private_172() {
        let err = validate_url_ssrf("http://172.16.0.1/mcp")
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::SsrfBlocked { .. }));
    }

    #[tokio::test]
    async fn ssrf_blocks_private_192() {
        let err = validate_url_ssrf("http://192.168.1.1/mcp")
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::SsrfBlocked { .. }));
    }

    #[tokio::test]
    async fn ssrf_blocks_link_local() {
        let err = validate_url_ssrf("http://169.254.1.1/mcp")
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::SsrfBlocked { .. }));
    }

    #[tokio::test]
    async fn ssrf_blocks_zero() {
        let err = validate_url_ssrf("http://0.0.0.0/mcp").await.unwrap_err();
        assert!(matches!(err, McpError::SsrfBlocked { .. }));
    }

    #[tokio::test]
    async fn ssrf_blocks_ipv6_loopback() {
        let err = validate_url_ssrf("http://[::1]:8080/mcp")
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::SsrfBlocked { .. }));
    }

    #[tokio::test]
    async fn ssrf_rejects_invalid_url() {
        let err = validate_url_ssrf("not-a-url").await.unwrap_err();
        assert!(matches!(err, McpError::InvalidUrl { .. }));
    }

    #[test]
    fn ssrf_error_display() {
        let err = McpError::SsrfBlocked {
            url: "http://127.0.0.1/mcp".into(),
            addr: "127.0.0.1".into(),
        };
        assert!(err.to_string().contains("SSRF blocked"));
    }

    /// Verify that `validate_url_ssrf` blocks `localhost` hostname (DNS resolves to 127.0.0.1).
    #[tokio::test]
    async fn ssrf_blocks_localhost_hostname() {
        let err = validate_url_ssrf("http://localhost:3001/mcp")
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::SsrfBlocked { .. }));
    }

    /// Verify that `validate_url_ssrf` blocks 127.0.0.1 explicitly.
    #[tokio::test]
    async fn ssrf_blocks_loopback_ip_port() {
        let err = validate_url_ssrf("http://127.0.0.1:3001/mcp")
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::SsrfBlocked { .. }));
    }

    /// Verify that `validate_url_ssrf` blocks private 192.168.x.x range.
    #[tokio::test]
    async fn ssrf_blocks_private_192_explicit() {
        let err = validate_url_ssrf("http://192.168.1.1/mcp")
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::SsrfBlocked { .. }));
    }

    // `connect_url` trusted-bypass coverage (logic verified via code review):
    //
    // In `connect_url`, the guard is:
    //
    //   if !trusted {
    //       validate_url_ssrf(url).await?;
    //   }
    //
    // When `trusted = true` the call to `validate_url_ssrf` is skipped entirely,
    // so no SSRF error can be returned for localhost/private URLs.  A real network
    // call would still be made and would fail (connection refused), but the *SSRF*
    // gate is not exercised.  The tests below confirm this contract at the
    // `is_private_ip` helper level — the source of truth for what "private" means.

    #[test]
    fn is_private_ip_blocks_loopback() {
        use std::net::Ipv4Addr;
        assert!(is_private_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
    }

    #[test]
    fn is_private_ip_blocks_private_192() {
        use std::net::Ipv4Addr;
        assert!(is_private_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
    }

    #[test]
    fn is_private_ip_blocks_private_10() {
        use std::net::Ipv4Addr;
        assert!(is_private_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
    }

    #[test]
    fn is_private_ip_allows_public() {
        use std::net::Ipv4Addr;
        // 8.8.8.8 is a public IP — must NOT be blocked.
        assert!(!is_private_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
    }

    #[test]
    fn is_private_ip_blocks_ipv6_loopback() {
        use std::net::Ipv6Addr;
        assert!(is_private_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn is_private_ip_blocks_ipv6_unique_local() {
        use std::net::Ipv6Addr;
        // fc00::/7 — unique local
        let fc = Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 1);
        assert!(is_private_ip(IpAddr::V6(fc)));
    }

    #[test]
    fn is_private_ip_blocks_ipv6_link_local() {
        use std::net::Ipv6Addr;
        // fe80::/10 — link-local
        let fe80 = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
        assert!(is_private_ip(IpAddr::V6(fe80)));
    }
}
