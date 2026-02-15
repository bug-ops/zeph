use std::borrow::Cow;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransport;
use tokio::process::Command;
use url::Url;

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
        timeout: Duration,
    ) -> Result<Self, McpError> {
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
    /// to private, loopback, or link-local IP ranges.
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
    ) -> Result<Self, McpError> {
        validate_url_ssrf(url).await?;

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

fn is_private_ip(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(ip) => {
            ip.is_loopback()              // 127.0.0.0/8
                || ip.is_private()        // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
                || ip.is_link_local()     // 169.254.0.0/16
                || ip.is_unspecified()    // 0.0.0.0
                || ip.is_broadcast() // 255.255.255.255
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()               // ::1
                || ip.is_unspecified()     // ::
                || ip.to_ipv4_mapped().is_some_and(|v4| {
                    v4.is_loopback() || v4.is_private() || v4.is_link_local()
                })                         // ::ffff:127.0.0.1 etc.
                || (ip.segments()[0] & 0xfe00) == 0xfc00   // fc00::/7 unique local
                || (ip.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
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
}
