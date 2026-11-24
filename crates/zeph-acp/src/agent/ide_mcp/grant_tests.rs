// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance tests for IDE MCP grants (#7081), the per-session cap (#7082) and the
//! two-connection ownership guarantee (#7086).
//!
//! Registered from `ide_mcp.rs` with `#[cfg(test)] mod grant_tests;`. Assumes
//! `scoped_entries(servers, session_id, elicitation_secs, &AcpIdeMcpConfig)`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Duration;

use acp::v1::schema::{EnvVariable, McpServer, McpServerHttp, McpServerStdio};
use parking_lot::Mutex as SyncMutex;
use zeph_config::{AcpIdeMcpConfig, AcpIdeMcpGrant, AcpIdeMcpGrantError, AcpIdeMcpTarget};

use super::*;

const SESSION: &str = "0123456789";

fn stdio_grant(command: &str, args: &[&str], env_keys: &[&str], tools: &[&str]) -> AcpIdeMcpGrant {
    AcpIdeMcpGrant::new(
        AcpIdeMcpTarget::Stdio {
            command: command.to_owned(),
            args: args.iter().map(|s| (*s).to_owned()).collect(),
            env_keys: env_keys
                .iter()
                .map(|s| (*s).to_owned())
                .collect::<BTreeSet<_>>(),
        },
        tools.iter().map(|s| (*s).to_owned()).collect(),
    )
    .expect("valid test grant")
}

fn http_grant(url: &str, tools: &[&str]) -> AcpIdeMcpGrant {
    AcpIdeMcpGrant::new(
        AcpIdeMcpTarget::Http {
            url: url.to_owned(),
        },
        tools.iter().map(|s| (*s).to_owned()).collect(),
    )
    .expect("valid test grant")
}

fn config(cap: usize, grants: Vec<(&str, AcpIdeMcpGrant)>) -> AcpIdeMcpConfig {
    AcpIdeMcpConfig {
        max_servers_per_session: cap,
        servers: grants
            .into_iter()
            .map(|(name, grant)| (name.to_owned(), grant))
            .collect::<BTreeMap<_, _>>(),
    }
}

fn stdio_server(name: &str, command: &str, args: &[&str], env: &[(&str, &str)]) -> McpServer {
    let mut stdio = McpServerStdio::new(name, command);
    stdio.args = args.iter().map(|s| (*s).to_owned()).collect();
    stdio.env = env.iter().map(|(k, v)| EnvVariable::new(*k, *v)).collect();
    McpServer::Stdio(stdio)
}

fn allowlist_of(servers: &[McpServer], cfg: &AcpIdeMcpConfig) -> Option<Vec<String>> {
    let entries = scoped_entries(servers, SESSION, 120, cfg);
    assert_eq!(entries.len(), 1, "exactly one entry expected");
    entries[0].tool_allowlist.clone()
}

fn fs_cfg() -> AcpIdeMcpConfig {
    config(
        10,
        vec![(
            "filesystem",
            stdio_grant(
                "npx",
                &["-y", "server-filesystem", "/repo"],
                &["GITHUB_TOKEN"],
                &["read_file"],
            ),
        )],
    )
}

#[test]
fn scoped_entries_exact_stdio_match_sets_allowlist() {
    let servers = [stdio_server(
        "filesystem",
        "npx",
        &["-y", "server-filesystem", "/repo"],
        &[("GITHUB_TOKEN", "v")],
    )];
    assert_eq!(
        allowlist_of(&servers, &fs_cfg()),
        Some(vec!["read_file".to_owned()])
    );
}

#[test]
fn scoped_entries_env_values_are_not_pinned() {
    let servers = [stdio_server(
        "filesystem",
        "npx",
        &["-y", "server-filesystem", "/repo"],
        &[("GITHUB_TOKEN", "another-value")],
    )];
    assert!(allowlist_of(&servers, &fs_cfg()).is_some());
}

#[test]
fn scoped_entries_blocklisted_env_is_stripped_before_the_subset_check() {
    let servers = [stdio_server(
        "filesystem",
        "npx",
        &["-y", "server-filesystem", "/repo"],
        &[("GITHUB_TOKEN", "v"), ("LD_PRELOAD", "/evil.so")],
    )];
    assert!(allowlist_of(&servers, &fs_cfg()).is_some());
}

#[test]
fn scoped_entries_empty_env_keys_allow_no_env_at_all() {
    let cfg = config(
        10,
        vec![("fs", stdio_grant("mcp-fs", &[], &[], &["read_file"]))],
    );
    let none = [stdio_server("fs", "mcp-fs", &[], &[])];
    let with_env = [stdio_server("fs", "mcp-fs", &[], &[("SOME_KEY", "x")])];
    assert!(allowlist_of(&none, &cfg).is_some());
    assert_eq!(allowlist_of(&with_env, &cfg), None);
}

#[test]
fn scoped_entries_args_mismatch_fails_closed() {
    let servers = [stdio_server(
        "filesystem",
        "npx",
        &["-y", "server-filesystem", "/"],
        &[],
    )];
    assert_eq!(allowlist_of(&servers, &fs_cfg()), None);
}

#[test]
fn scoped_entries_command_mismatch_fails_closed() {
    let servers = [stdio_server(
        "filesystem",
        "/tmp/evil",
        &["-y", "server-filesystem", "/repo"],
        &[],
    )];
    assert_eq!(allowlist_of(&servers, &fs_cfg()), None);
}

#[test]
fn scoped_entries_extra_env_key_fails_closed() {
    let servers = [stdio_server(
        "filesystem",
        "npx",
        &["-y", "server-filesystem", "/repo"],
        &[
            ("GITHUB_TOKEN", "v"),
            ("npm_config_registry", "http://evil"),
        ],
    )];
    assert_eq!(allowlist_of(&servers, &fs_cfg()), None);
}

#[test]
fn scoped_entries_without_grant_for_the_name_fails_closed() {
    let servers = [stdio_server("unknown", "npx", &[], &[])];
    assert_eq!(allowlist_of(&servers, &fs_cfg()), None);
}

#[test]
fn scoped_entries_default_config_never_grants() {
    let servers = [stdio_server("filesystem", "npx", &[], &[])];
    assert_eq!(allowlist_of(&servers, &AcpIdeMcpConfig::default()), None);
}

#[test]
fn scoped_entries_http_url_must_match_exactly() {
    let cfg = config(
        10,
        vec![(
            "docs",
            http_grant("https://mcp.example.com/mcp", &["search"]),
        )],
    );
    let same = [McpServer::Http(McpServerHttp::new(
        "docs",
        "https://mcp.example.com/mcp",
    ))];
    let other = [McpServer::Http(McpServerHttp::new(
        "docs",
        "https://mcp.example.com/other",
    ))];
    assert_eq!(allowlist_of(&same, &cfg), Some(vec!["search".to_owned()]));
    assert_eq!(allowlist_of(&other, &cfg), None);
}

#[test]
fn scoped_entries_transport_kind_mismatch_fails_closed() {
    let cfg = config(
        10,
        vec![(
            "docs",
            http_grant("https://mcp.example.com/mcp", &["search"]),
        )],
    );
    let stdio = [stdio_server("docs", "npx", &[], &[])];
    assert_eq!(allowlist_of(&stdio, &cfg), None);
}

#[test]
fn scoped_entries_granted_entries_stay_untrusted_without_all_tools_mode() {
    let servers = [stdio_server(
        "filesystem",
        "npx",
        &["-y", "server-filesystem", "/repo"],
        &[],
    )];
    let entries = scoped_entries(&servers, SESSION, 120, &fs_cfg());
    assert_eq!(entries[0].trust_level, zeph_mcp::McpTrustLevel::Untrusted);
    assert!(!entries[0].allow_untrusted_without_allowlist);
}

#[test]
fn scoped_entries_cap_truncates_to_the_first_n_after_dedup() {
    let servers = [
        stdio_server("one", "/bin/a", &[], &[]),
        stdio_server("one", "/bin/dup", &[], &[]),
        stdio_server("two", "/bin/b", &[], &[]),
        stdio_server("three", "/bin/c", &[], &[]),
    ];
    let entries = scoped_entries(&servers, SESSION, 120, &config(2, vec![]));
    let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["one-01234567", "two-01234567"]);
}

#[test]
fn scoped_entries_cap_zero_disables_ide_servers() {
    let servers = [stdio_server("one", "/bin/a", &[], &[])];
    assert!(scoped_entries(&servers, SESSION, 120, &config(0, vec![])).is_empty());
}

#[test]
fn loopback_http_grant_is_rejected_at_construction() {
    let target = AcpIdeMcpTarget::Http {
        url: "http://127.0.0.1:9/mcp".to_owned(),
    };
    let err = AcpIdeMcpGrant::new(target, vec!["search".to_owned()]).unwrap_err();
    assert!(matches!(err, AcpIdeMcpGrantError::UnreachableUrl(_)));
}

#[derive(Default)]
struct SharedHost {
    connected: SyncMutex<HashSet<String>>,
    removed: SyncMutex<Vec<String>>,
}

impl IdeMcpHost for SharedHost {
    fn add_server(
        &self,
        entry: &ServerEntry,
    ) -> impl Future<Output = Result<Vec<McpTool>, McpError>> + Send {
        let result = if self.connected.lock().insert(entry.id.to_string()) {
            Ok(Vec::new())
        } else {
            Err(McpError::ServerAlreadyConnected {
                server_id: entry.id.to_string(),
            })
        };
        std::future::ready(result)
    }

    fn remove_server(&self, server_id: &str) -> impl Future<Output = Result<(), McpError>> + Send {
        self.removed.lock().push(server_id.to_owned());
        let result = if self.connected.lock().remove(server_id) {
            Ok(())
        } else {
            Err(McpError::ServerNotFound {
                server_id: server_id.to_owned(),
            })
        };
        std::future::ready(result)
    }
}

fn plain_entry(id: &str) -> ServerEntry {
    ServerEntry {
        id: id.into(),
        transport: zeph_mcp::McpTransport::Stdio {
            command: "unused".to_owned(),
            args: Vec::new(),
            env: HashMap::new(),
        },
        timeout: Duration::from_secs(1),
        trust_level: zeph_mcp::McpTrustLevel::Untrusted,
        tool_allowlist: None,
        allow_untrusted_without_allowlist: false,
        expected_tools: Vec::new(),
        roots: Vec::new(),
        tool_metadata: HashMap::new(),
        elicitation_enabled: false,
        elicitation_timeout_secs: 120,
        env_isolation: false,
        media_passthrough: false,
    }
}

/// #7086: the second connection's attach of an already-attached scoped id must neither remove
/// nor re-add the first connection's server, and the first lease still releases it.
#[tokio::test]
async fn second_connection_attach_of_same_scoped_id_leaves_first_connection_intact() {
    let host = SharedHost::default();
    let first_conn = IdeMcpRegistry::new();
    let second_conn = IdeMcpRegistry::new();

    let first = first_conn
        .attach(&host, vec![plain_entry("ide-01234567")])
        .await;
    let first_lease = first.lease.expect("first connection owns the server");

    let second = second_conn
        .attach(&host, vec![plain_entry("ide-01234567")])
        .await;
    assert!(
        second.lease.is_none(),
        "second connection must get no lease"
    );
    assert!(host.removed.lock().is_empty(), "nothing may be removed");
    assert!(host.connected.lock().contains("ide-01234567"));

    drop(second.lease);
    drop(first_lease);
    let request = first_conn
        .release_rx
        .lock()
        .await
        .recv()
        .await
        .expect("first lease queues its release");
    first_conn.release(&host, request).await;
    assert!(!host.connected.lock().contains("ide-01234567"));
    assert_eq!(*host.removed.lock(), ["ide-01234567"]);
}

#[test]
#[tracing_test::traced_test]
fn scoped_entries_cap_truncation_logs_requested_cap_and_dropped() {
    let servers = [
        McpServer::Http(McpServerHttp::new("a", "https://a.example.com")),
        McpServer::Http(McpServerHttp::new("b", "https://b.example.com")),
        McpServer::Http(McpServerHttp::new("c", "https://c.example.com")),
    ];
    let entries = scoped_entries(&servers, SESSION, 120, &config(1, Vec::new()));
    assert_eq!(entries.len(), 1);
    assert!(logs_contain("exceed [acp.ide_mcp].max_servers_per_session"));
    assert!(logs_contain("requested=3"));
    assert!(logs_contain("cap=1"));
    assert!(logs_contain("dropped=2"));
}

#[test]
#[tracing_test::traced_test]
fn granted_allowlist_mismatch_logs_warning_with_server_name() {
    let cfg = config(
        10,
        vec![(
            "docs",
            http_grant("https://mcp.example.com/mcp", &["search"]),
        )],
    );
    let servers = [McpServer::Http(McpServerHttp::new(
        "docs",
        "https://evil.example.com/mcp",
    ))];
    assert_eq!(allowlist_of(&servers, &cfg), None);
    assert!(logs_contain("does not match its operator grant"));
    assert!(logs_contain("server=docs"));
}
