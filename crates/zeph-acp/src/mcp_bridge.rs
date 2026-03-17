// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::time::Duration;

use agent_client_protocol as acp;
use zeph_mcp::{McpTransport, ServerEntry};

const DEFAULT_MCP_TIMEOUT_SECS: u64 = 30;

/// Convert ACP `McpServer` list to `zeph-mcp` `ServerEntry` configs.
///
/// `Stdio`, `Http`, and `Sse` transports are supported. `Sse` is mapped to
/// `McpTransport::Http` since rmcp's `StreamableHttpClientTransport` handles both.
#[must_use]
pub fn acp_mcp_servers_to_entries(servers: &[acp::McpServer]) -> Vec<ServerEntry> {
    servers
        .iter()
        .filter_map(|s| match s {
            acp::McpServer::Stdio(stdio) => {
                // IDE is the trusted client in the stdio transport model; env vars are passed
                // as-is to the MCP server child process without further sanitization.
                let env: HashMap<String, String> = stdio
                    .env
                    .iter()
                    .map(|e| (e.name.clone(), e.value.clone()))
                    .collect();
                Some(ServerEntry {
                    id: stdio.name.clone(),
                    transport: McpTransport::Stdio {
                        command: stdio.command.display().to_string(),
                        args: stdio.args.clone(),
                        env,
                    },
                    timeout: Duration::from_secs(DEFAULT_MCP_TIMEOUT_SECS),
                    trusted: false,
                })
            }
            acp::McpServer::Http(http) => Some(ServerEntry {
                id: http.name.clone(),
                transport: McpTransport::Http {
                    url: http.url.clone(),
                    headers: std::collections::HashMap::new(),
                },
                timeout: Duration::from_secs(DEFAULT_MCP_TIMEOUT_SECS),
                trusted: false,
            }),
            acp::McpServer::Sse(sse) => {
                // SSE is a legacy MCP transport; map to Streamable HTTP which is
                // backward-compatible. rmcp's StreamableHttpClientTransport handles both.
                Some(ServerEntry {
                    id: sse.name.clone(),
                    transport: McpTransport::Http {
                        url: sse.url.clone(),
                        headers: std::collections::HashMap::new(),
                    },
                    timeout: Duration::from_secs(DEFAULT_MCP_TIMEOUT_SECS),
                    trusted: false,
                })
            }
            _ => {
                tracing::warn!("skipping unknown MCP server transport — not supported");
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_stdio_server() {
        let servers = vec![acp::McpServer::Stdio(acp::McpServerStdio::new(
            "my-mcp",
            "/usr/bin/my-mcp",
        ))];
        let entries = acp_mcp_servers_to_entries(&servers);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "my-mcp");
        assert!(matches!(entries[0].transport, McpTransport::Stdio { .. }));
    }

    #[test]
    fn converts_http_server() {
        let servers = vec![acp::McpServer::Http(acp::McpServerHttp::new(
            "http-mcp",
            "http://localhost",
        ))];
        let entries = acp_mcp_servers_to_entries(&servers);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "http-mcp");
        assert!(matches!(entries[0].transport, McpTransport::Http { .. }));
    }

    #[test]
    fn converts_http_server_url() {
        let servers = vec![acp::McpServer::Http(acp::McpServerHttp::new(
            "http-mcp",
            "http://example.com:8080/mcp",
        ))];
        let entries = acp_mcp_servers_to_entries(&servers);
        if let McpTransport::Http { url, .. } = &entries[0].transport {
            assert_eq!(url, "http://example.com:8080/mcp");
        } else {
            panic!("expected Http transport");
        }
    }

    #[test]
    fn converts_env_variables() {
        let stdio = acp::McpServerStdio::new("env-mcp", "/bin/mcp").env(vec![
            acp::EnvVariable::new("FOO", "bar"),
            acp::EnvVariable::new("BAZ", "qux"),
        ]);
        let entries = acp_mcp_servers_to_entries(&[acp::McpServer::Stdio(stdio)]);
        if let McpTransport::Stdio { env, .. } = &entries[0].transport {
            assert_eq!(env.get("FOO"), Some(&"bar".to_owned()));
            assert_eq!(env.get("BAZ"), Some(&"qux".to_owned()));
        } else {
            panic!("expected Stdio transport");
        }
    }

    #[test]
    fn empty_input_returns_empty() {
        assert!(acp_mcp_servers_to_entries(&[]).is_empty());
    }

    #[test]
    fn converts_sse_server() {
        let servers = vec![acp::McpServer::Sse(acp::McpServerSse::new(
            "sse-mcp",
            "http://localhost/sse",
        ))];
        let entries = acp_mcp_servers_to_entries(&servers);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "sse-mcp");
        assert!(matches!(entries[0].transport, McpTransport::Http { .. }));
    }

    #[test]
    fn converts_sse_server_url() {
        let servers = vec![acp::McpServer::Sse(acp::McpServerSse::new(
            "sse-mcp",
            "http://example.com/sse",
        ))];
        let entries = acp_mcp_servers_to_entries(&servers);
        if let McpTransport::Http { url, .. } = &entries[0].transport {
            assert_eq!(url, "http://example.com/sse");
        } else {
            panic!("expected Http transport");
        }
    }

    #[test]
    fn mixed_list_returns_all() {
        let servers = vec![
            acp::McpServer::Stdio(acp::McpServerStdio::new("stdio-1", "/bin/mcp1")),
            acp::McpServer::Http(acp::McpServerHttp::new("http-1", "http://localhost")),
            acp::McpServer::Stdio(acp::McpServerStdio::new("stdio-2", "/bin/mcp2")),
            acp::McpServer::Sse(acp::McpServerSse::new("sse-1", "http://localhost/sse")),
        ];
        let entries = acp_mcp_servers_to_entries(&servers);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].id, "stdio-1");
        assert_eq!(entries[1].id, "http-1");
        assert_eq!(entries[2].id, "stdio-2");
        assert_eq!(entries[3].id, "sse-1");
    }
}
