// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance tests for the `[acp.ide_mcp]` grant config (#7081, #7082) and for the typed
//! `McpServerConfig.id` (#7092).

use std::collections::BTreeSet;

use zeph_common::types::McpServerId;
use zeph_config::{AcpConfig, AcpIdeMcpTarget, McpServerConfig};

fn parse(toml_src: &str) -> Result<AcpConfig, toml::de::Error> {
    toml::from_str(toml_src)
}

#[test]
fn acp_config_ide_mcp_defaults_to_cap_10_and_no_grants() {
    let cfg = parse("").expect("empty [acp] parses");
    assert_eq!(cfg.ide_mcp.max_servers_per_session, 10);
    assert!(cfg.ide_mcp.servers.is_empty());
}

#[test]
fn acp_config_ide_mcp_stdio_grant_parses_with_env_keys() {
    let cfg = parse(
        r#"
        [ide_mcp]
        max_servers_per_session = 2

        [ide_mcp.servers.filesystem]
        command = "npx"
        args = ["-y", "@modelcontextprotocol/server-filesystem", "/repo"]
        env_keys = ["GITHUB_TOKEN"]
        tools = ["read_file"]
        "#,
    )
    .expect("valid stdio grant");
    assert_eq!(cfg.ide_mcp.max_servers_per_session, 2);
    let grant = &cfg.ide_mcp.servers["filesystem"];
    assert_eq!(grant.tools, ["read_file"]);
    match &grant.target {
        AcpIdeMcpTarget::Stdio {
            command,
            args,
            env_keys,
        } => {
            assert_eq!(command, "npx");
            assert_eq!(args.len(), 3);
            assert_eq!(*env_keys, BTreeSet::from(["GITHUB_TOKEN".to_owned()]));
        }
        other @ AcpIdeMcpTarget::Http { .. } => panic!("expected Stdio, got {other:?}"),
    }
}

#[test]
fn acp_config_ide_mcp_http_grant_parses() {
    let cfg = parse(
        r#"
        [ide_mcp.servers.docs]
        url = "https://mcp.example.com/mcp"
        tools = ["search"]
        "#,
    )
    .expect("valid http grant");
    match &cfg.ide_mcp.servers["docs"].target {
        AcpIdeMcpTarget::Http { url } => assert_eq!(url, "https://mcp.example.com/mcp"),
        other @ AcpIdeMcpTarget::Stdio { .. } => panic!("expected Http, got {other:?}"),
    }
}

#[test]
fn acp_config_ide_mcp_stdio_grant_without_env_keys_means_empty_set() {
    let cfg = parse(
        r#"
        [ide_mcp.servers.fs]
        command = "mcp-fs"
        tools = ["read_file"]
        "#,
    )
    .expect("valid grant");
    match &cfg.ide_mcp.servers["fs"].target {
        AcpIdeMcpTarget::Stdio { env_keys, args, .. } => {
            assert!(env_keys.is_empty());
            assert_eq!(args.as_slice(), [] as [String; 0]);
        }
        other @ AcpIdeMcpTarget::Http { .. } => panic!("expected Stdio, got {other:?}"),
    }
}

#[test]
fn acp_config_ide_mcp_grant_with_both_command_and_url_is_rejected() {
    let err = parse(
        r#"
        [ide_mcp.servers.x]
        command = "npx"
        url = "https://mcp.example.com/mcp"
        tools = ["t"]
        "#,
    );
    assert!(err.is_err());
}

#[test]
fn acp_config_ide_mcp_grant_with_neither_command_nor_url_is_rejected() {
    let err = parse(
        r#"
        [ide_mcp.servers.x]
        tools = ["t"]
        "#,
    );
    assert!(err.is_err());
}

#[test]
fn acp_config_ide_mcp_grant_with_url_and_args_is_rejected() {
    let err = parse(
        r#"
        [ide_mcp.servers.x]
        url = "https://mcp.example.com/mcp"
        args = ["--flag"]
        tools = ["t"]
        "#,
    );
    assert!(err.is_err());
}

#[test]
fn acp_config_ide_mcp_grant_with_url_and_env_keys_is_rejected() {
    let err = parse(
        r#"
        [ide_mcp.servers.x]
        url = "https://mcp.example.com/mcp"
        env_keys = ["TOKEN"]
        tools = ["t"]
        "#,
    );
    assert!(err.is_err());
}

#[test]
fn mcp_server_config_id_is_a_typed_server_id_and_round_trips_through_toml() {
    let cfg: McpServerConfig =
        toml::from_str("id = \"gh\"\ncommand = \"npx\"\n").expect("minimal server config");
    let id: &McpServerId = &cfg.id;
    assert_eq!(id.as_str(), "gh");

    let rendered = toml::to_string(&cfg).expect("serializes");
    assert!(rendered.contains("id = \"gh\""), "got: {rendered}");
    let again: McpServerConfig = toml::from_str(&rendered).expect("re-parses");
    assert_eq!(again.id, cfg.id);
}
