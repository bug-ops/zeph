// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Verifies that the `max_connect_attempts` key is present in all three `default.toml` copies.
//!
//! The three default config files are not byte-identical (they have different verbosity levels),
//! but every key added to `McpConfig` must appear in all three as a commented-out default so
//! users can discover it regardless of which config they start from.

#[test]
fn default_toml_mcp_section_has_max_connect_attempts() {
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent() // zeph-config → crates
        .and_then(|p| p.parent()) // crates → workspace root
        .expect("failed to find workspace root");

    let paths = [
        "config/default.toml",
        "crates/zeph-core/config/default.toml",
        "crates/zeph-config/config/default.toml",
    ];

    for rel_path in &paths {
        let full_path = workspace_root.join(rel_path);
        let content = std::fs::read_to_string(&full_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", full_path.display()));

        assert!(
            content.contains("max_connect_attempts"),
            "missing 'max_connect_attempts' in {}",
            full_path.display()
        );
    }
}
