// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Conversion between ACP `McpServer` descriptors and `zeph-mcp` `ServerEntry` configs.
//!
//! IDEs send MCP server configurations inside `new_session`. This module converts
//! those ACP types into the `ServerEntry` format understood by [`zeph_mcp::McpManager`].
//!
//! # Security
//!
//! Dangerous environment variables (library injection, PATH hijacking, proxy
//! interception, shell startup injection, runtime module injection) are stripped
//! from stdio MCP server configs before they are passed to child processes.

use std::collections::HashMap;
use std::time::Duration;

use agent_client_protocol as acp;
use zeph_config::McpTrustLevel;
use zeph_mcp::{McpTransport, ServerEntry};

const DEFAULT_MCP_TIMEOUT_SECS: u64 = 30;

/// Convert an ACP `McpServer` list to `zeph-mcp` [`ServerEntry`] configs.
///
/// `Stdio`, `Http`, and `Sse` transports are supported. `Sse` is mapped to
/// `McpTransport::Http` since rmcp's `StreamableHttpClientTransport` handles both.
/// Unknown transport variants are skipped with a warning.
///
/// All converted entries start with [`McpTrustLevel::Untrusted`] and no tool allowlist
/// so that the agent sandbox applies to IDE-requested MCP servers.
///
/// `elicitation_enabled` is read from the `_meta` field of each server entry under
/// the key `"elicitation_enabled"`. Absent or non-boolean values default to `false`.
///
/// `elicitation_default_timeout_secs` is used as the fallback when `elicitation_timeout_secs`
/// is absent from `_meta`. Pass `[acp.timeouts].elicitation_secs` from the agent config.
///
/// # Security
///
/// Dangerous environment variables are stripped from `Stdio` server configs.
/// See module-level documentation for the full blocklist.
///
/// # Examples
///
/// ```
/// use agent_client_protocol::schema::v1::{McpServer, McpServerStdio};
/// use zeph_acp::acp_mcp_servers_to_entries;
///
/// let servers = vec![
///     McpServer::Stdio(McpServerStdio::new("my-server", "/usr/bin/my-mcp")),
/// ];
/// let entries = acp_mcp_servers_to_entries(&servers, 120);
/// assert_eq!(entries.len(), 1);
/// assert_eq!(entries[0].id, "my-server");
/// ```
#[must_use]
pub fn acp_mcp_servers_to_entries(
    servers: &[acp::schema::v1::McpServer],
    elicitation_default_timeout_secs: u64,
) -> Vec<ServerEntry> {
    servers
        .iter()
        .filter_map(|s| match s {
            acp::schema::v1::McpServer::Stdio(stdio) => {
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
                    roots: Vec::new(),
                    tool_metadata: HashMap::new(),
                    elicitation_enabled: elicitation_from_meta(stdio.meta.as_ref()),
                    elicitation_timeout_secs: elicitation_timeout_from_meta(
                        stdio.meta.as_ref(),
                        elicitation_default_timeout_secs,
                    ),
                    env_isolation: false,
                    media_passthrough: false,
                })
            }
            acp::schema::v1::McpServer::Http(http) => Some(ServerEntry {
                id: http.name.clone(),
                transport: McpTransport::Http {
                    url: http.url.clone(),
                    headers: std::collections::HashMap::new(),
                },
                timeout: Duration::from_secs(DEFAULT_MCP_TIMEOUT_SECS),
                trust_level: McpTrustLevel::Untrusted,
                tool_allowlist: None,
                expected_tools: Vec::new(),
                roots: Vec::new(),
                tool_metadata: HashMap::new(),
                elicitation_enabled: elicitation_from_meta(http.meta.as_ref()),
                elicitation_timeout_secs: elicitation_timeout_from_meta(
                    http.meta.as_ref(),
                    elicitation_default_timeout_secs,
                ),
                env_isolation: false,
                media_passthrough: false,
            }),
            acp::schema::v1::McpServer::Sse(sse) => {
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
                    roots: Vec::new(),
                    tool_metadata: HashMap::new(),
                    elicitation_enabled: elicitation_from_meta(sse.meta.as_ref()),
                    elicitation_timeout_secs: elicitation_timeout_from_meta(
                        sse.meta.as_ref(),
                        elicitation_default_timeout_secs,
                    ),
                    env_isolation: false,
                    media_passthrough: false,
                })
            }
            _ => {
                tracing::warn!("skipping unknown MCP server transport — not supported");
                None
            }
        })
        .collect()
}

/// Read `elicitation_enabled` from the ACP `_meta` map.
///
/// IDEs pass `elicitation_enabled: true` inside the `_meta` field of an MCP server entry
/// to opt-in to MCP elicitation support for that server. Absent or non-boolean values
/// default to `false` so existing clients are unaffected.
fn elicitation_from_meta(meta: Option<&serde_json::Map<String, serde_json::Value>>) -> bool {
    meta.and_then(|m| m.get("elicitation_enabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Read `elicitation_timeout_secs` from the ACP `_meta` map.
///
/// Returns the u64 timeout value when present and valid, otherwise falls back to
/// `default_secs` from `[acp.timeouts]` configuration.
pub(crate) fn elicitation_timeout_from_meta(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
    default_secs: u64,
) -> u64 {
    meta.and_then(|m| m.get("elicitation_timeout_secs"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(default_secs)
}

/// Env vars that must never be passed from ACP clients to MCP child processes.
/// These enable library injection, path hijacking, proxy interception, and other privilege
/// escalation vectors.
fn is_dangerous_env_var(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        // Library injection (Linux / macOS)
        "LD_PRELOAD"
            | "LD_LIBRARY_PATH"
            | "DYLD_INSERT_LIBRARIES"
            | "DYLD_LIBRARY_PATH"
            | "DYLD_FRAMEWORK_PATH"
            | "DYLD_FALLBACK_LIBRARY_PATH"
            // Path hijacking — attacker-controlled PATH redirects binary execution
            | "PATH"
            // Network proxy interception
            | "HTTP_PROXY"
            | "HTTPS_PROXY"
            | "ALL_PROXY"
            | "NO_PROXY"
            // Shell startup injection — executed by bash/sh unconditionally on startup
            | "BASH_ENV"
            | "ENV"
            // Interpreted-runtime module injection
            | "PYTHONPATH"
            | "NODE_PATH"
            | "RUBYLIB"
    )
}

#[cfg(test)]
mod elicitation_tests {
    use super::elicitation_from_meta;

    #[test]
    fn absent_meta_returns_false() {
        assert!(!elicitation_from_meta(None));
    }

    #[test]
    fn missing_key_returns_false() {
        let mut map = serde_json::Map::new();
        map.insert("other_key".to_owned(), serde_json::Value::Bool(true));
        assert!(!elicitation_from_meta(Some(&map)));
    }

    #[test]
    fn key_true_returns_true() {
        let mut map = serde_json::Map::new();
        map.insert(
            "elicitation_enabled".to_owned(),
            serde_json::Value::Bool(true),
        );
        assert!(elicitation_from_meta(Some(&map)));
    }

    #[test]
    fn key_false_returns_false() {
        let mut map = serde_json::Map::new();
        map.insert(
            "elicitation_enabled".to_owned(),
            serde_json::Value::Bool(false),
        );
        assert!(!elicitation_from_meta(Some(&map)));
    }

    #[test]
    fn non_bool_value_returns_false() {
        let mut map = serde_json::Map::new();
        map.insert(
            "elicitation_enabled".to_owned(),
            serde_json::Value::String("true".to_owned()),
        );
        assert!(!elicitation_from_meta(Some(&map)));
    }
}

#[cfg(test)]
mod elicitation_timeout_tests {
    use super::elicitation_timeout_from_meta;

    #[test]
    fn absent_meta_returns_default() {
        assert_eq!(elicitation_timeout_from_meta(None, 120), 120);
    }

    #[test]
    fn missing_key_returns_default() {
        let mut map = serde_json::Map::new();
        map.insert("other_key".to_owned(), serde_json::Value::Bool(true));
        assert_eq!(elicitation_timeout_from_meta(Some(&map), 120), 120);
    }

    #[test]
    fn valid_u64_value_returned() {
        let mut map = serde_json::Map::new();
        map.insert(
            "elicitation_timeout_secs".to_owned(),
            serde_json::Value::Number(60.into()),
        );
        assert_eq!(elicitation_timeout_from_meta(Some(&map), 120), 60);
    }

    #[test]
    fn non_numeric_value_returns_default() {
        let mut map = serde_json::Map::new();
        map.insert(
            "elicitation_timeout_secs".to_owned(),
            serde_json::Value::String("60".to_owned()),
        );
        assert_eq!(elicitation_timeout_from_meta(Some(&map), 120), 120);
    }

    #[test]
    fn negative_value_returns_default() {
        let mut map = serde_json::Map::new();
        map.insert("elicitation_timeout_secs".to_owned(), serde_json::json!(-1));
        assert_eq!(elicitation_timeout_from_meta(Some(&map), 120), 120);
    }

    #[test]
    fn custom_default_is_used_when_meta_absent() {
        assert_eq!(elicitation_timeout_from_meta(None, 300), 300);
    }
}
