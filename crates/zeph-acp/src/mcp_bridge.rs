// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::time::Duration;

use agent_client_protocol as acp;
use zeph_mcp::{McpTransport, McpTrustLevel, ServerEntry};

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
                let env: HashMap<String, String> = stdio
                    .env
                    .iter()
                    .filter(|e| !is_dangerous_env_var(&e.name))
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
                    trust_level: McpTrustLevel::Untrusted,
                    tool_allowlist: None,
                    expected_tools: Vec::new(),
                })
            }
            acp::McpServer::Http(http) => Some(ServerEntry {
                id: http.name.clone(),
                transport: McpTransport::Http {
                    url: http.url.clone(),
                    headers: std::collections::HashMap::new(),
                },
                timeout: Duration::from_secs(DEFAULT_MCP_TIMEOUT_SECS),
                trust_level: McpTrustLevel::Untrusted,
                tool_allowlist: None,
                expected_tools: Vec::new(),
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
                    trust_level: McpTrustLevel::Untrusted,
                    tool_allowlist: None,
                    expected_tools: Vec::new(),
                })
            }
            _ => {
                tracing::warn!("skipping unknown MCP server transport — not supported");
                None
            }
        })
        .collect()
}

/// Env vars that must never be passed from ACP clients to MCP child processes.
/// These enable library injection, path hijacking, and other privilege escalation vectors.
fn is_dangerous_env_var(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "LD_PRELOAD"
            | "LD_LIBRARY_PATH"
            | "DYLD_INSERT_LIBRARIES"
            | "DYLD_LIBRARY_PATH"
            | "DYLD_FRAMEWORK_PATH"
            | "DYLD_FALLBACK_LIBRARY_PATH"
    )
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

    #[test]
    fn dangerous_env_vars_stripped() {
        let stdio = acp::McpServerStdio::new("env-mcp", "/bin/mcp").env(vec![
            acp::EnvVariable::new("SAFE_VAR", "ok"),
            acp::EnvVariable::new("LD_PRELOAD", "/tmp/evil.so"),
            acp::EnvVariable::new("DYLD_INSERT_LIBRARIES", "/tmp/evil.dylib"),
            acp::EnvVariable::new("LD_LIBRARY_PATH", "/tmp"),
        ]);
        let entries = acp_mcp_servers_to_entries(&[acp::McpServer::Stdio(stdio)]);
        if let McpTransport::Stdio { env, .. } = &entries[0].transport {
            assert_eq!(env.get("SAFE_VAR"), Some(&"ok".to_owned()));
            assert!(env.get("LD_PRELOAD").is_none());
            assert!(env.get("DYLD_INSERT_LIBRARIES").is_none());
            assert!(env.get("LD_LIBRARY_PATH").is_none());
        } else {
            panic!("expected Stdio transport");
        }
    }

    #[test]
    fn acp_servers_have_none_allowlist() {
        let servers = vec![
            acp::McpServer::Stdio(acp::McpServerStdio::new("s", "/bin/s")),
            acp::McpServer::Http(acp::McpServerHttp::new("h", "http://localhost")),
            acp::McpServer::Sse(acp::McpServerSse::new("e", "http://localhost/sse")),
        ];
        let entries = acp_mcp_servers_to_entries(&servers);
        for entry in &entries {
            assert!(
                entry.tool_allowlist.is_none(),
                "ACP-requested server '{}' must have tool_allowlist=None",
                entry.id
            );
        }
    }

    #[test]
    fn is_dangerous_env_var_cases() {
        assert!(super::is_dangerous_env_var("LD_PRELOAD"));
        assert!(super::is_dangerous_env_var("ld_preload"));
        assert!(super::is_dangerous_env_var("DYLD_INSERT_LIBRARIES"));
        assert!(super::is_dangerous_env_var("DYLD_LIBRARY_PATH"));
        assert!(super::is_dangerous_env_var("DYLD_FRAMEWORK_PATH"));
        assert!(super::is_dangerous_env_var("DYLD_FALLBACK_LIBRARY_PATH"));
        assert!(!super::is_dangerous_env_var("PATH"));
        assert!(!super::is_dangerous_env_var("HOME"));
        assert!(!super::is_dangerous_env_var("MY_VAR"));
    }
}
