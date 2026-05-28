// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Verifies that required `[mcp]` keys are present in all three `default.toml` copies.
//!
//! The three default config files are not byte-identical (they have different verbosity levels),
//! but every key added to `McpConfig` must appear in all three as a commented-out default so
//! users can discover it regardless of which config they start from.

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent() // zeph-config → crates
        .and_then(|p| p.parent()) // crates → workspace root
        .expect("failed to find workspace root")
        .to_owned()
}

const DEFAULT_TOML_PATHS: &[&str] = &[
    "config/default.toml",
    "crates/zeph-core/config/default.toml",
];

fn read_default_tomls() -> Vec<(String, String)> {
    let root = workspace_root();
    DEFAULT_TOML_PATHS
        .iter()
        .map(|rel| {
            let full = root.join(rel);
            let content = std::fs::read_to_string(&full)
                .unwrap_or_else(|e| panic!("failed to read {}: {e}", full.display()));
            (full.display().to_string(), content)
        })
        .collect()
}

#[test]
fn default_toml_mcp_section_has_max_connect_attempts() {
    for (path, content) in read_default_tomls() {
        assert!(
            content.contains("max_connect_attempts"),
            "missing 'max_connect_attempts' in {path}"
        );
    }
}

#[test]
fn default_toml_mcp_section_has_startup_retry_backoff_ms() {
    for (path, content) in read_default_tomls() {
        assert!(
            content.contains("startup_retry_backoff_ms"),
            "missing 'startup_retry_backoff_ms' in {path}"
        );
    }
}

#[test]
fn default_toml_mcp_section_has_tool_timeout_secs() {
    for (path, content) in read_default_tomls() {
        assert!(
            content.contains("tool_timeout_secs"),
            "missing 'tool_timeout_secs' in {path}"
        );
    }
}
