// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance tests for `migrate_acp_ide_mcp_config` (#7081, #7082).
//!
//! The pipeline-order list in `migrate/tests.rs` (the step-name array near
//! `migrate_acp_auth_clients_config`) must also gain `"migrate_acp_ide_mcp_config"`.

use zeph_config::AcpConfig;
use zeph_config::migrate::migrate_acp_ide_mcp_config;

#[test]
fn migrate_acp_ide_mcp_adds_commented_block_when_absent() {
    let src = "[acp]\nenabled = true\n";
    let result = migrate_acp_ide_mcp_config(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(result.sections_changed.contains(&"acp.ide_mcp".to_owned()));
    assert!(result.output.contains("# [acp.ide_mcp]"));
    assert!(result.output.contains("max_servers_per_session"));
}

#[test]
fn migrate_acp_ide_mcp_output_stays_a_valid_config() {
    let result = migrate_acp_ide_mcp_config("[acp]\nenabled = true\n").expect("migrate");
    let cfg: AcpConfig = toml::from_str(
        result
            .output
            .split_once("[acp]\n")
            .expect("acp table kept")
            .1,
    )
    .expect("migrated acp table still parses");
    assert!(cfg.enabled);
    assert_eq!(cfg.ide_mcp.max_servers_per_session, 10);
}

#[test]
fn migrate_acp_ide_mcp_is_idempotent() {
    let once = migrate_acp_ide_mcp_config("[acp]\nenabled = true\n").expect("migrate");
    let twice = migrate_acp_ide_mcp_config(&once.output).expect("migrate again");
    assert_eq!(twice.changed_count, 0);
    assert_eq!(twice.output, once.output);
}

#[test]
fn migrate_acp_ide_mcp_skips_when_live_section_present() {
    let src = "[acp]\nenabled = true\n[acp.ide_mcp]\nmax_servers_per_session = 3\n";
    let result = migrate_acp_ide_mcp_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}

#[test]
fn migrate_acp_ide_mcp_skips_when_commented_section_present() {
    let src = "[acp]\nenabled = true\n# [acp.ide_mcp]\n# max_servers_per_session = 3\n";
    let result = migrate_acp_ide_mcp_config(src).expect("migrate");
    assert_eq!(result.changed_count, 0);
    assert_eq!(result.output, src);
}
