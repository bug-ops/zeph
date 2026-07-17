// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Quarantine-denied tool list shared between `zeph-tools` and `zeph-skills`.
//!
//! [`QUARANTINE_DENIED`] is the single source of truth for tools that are blocked when a
//! skill operates at the [`crate::SkillTrustLevel::Quarantined`] level.

/// Tools denied when a Quarantined skill is active.
///
/// Uses the actual tool IDs registered by `FileExecutor` and other executors.
/// MCP tools use a server-prefixed ID (e.g. `filesystem_write_file`). [`is_quarantine_denied`]
/// below checks both exact matches and `_{entry}` suffix matches to cover MCP-wrapped
/// versions of these native tool IDs.
///
/// Public so that `zeph-skills::scanner::check_capability_escalation` can use
/// this as the single source of truth for quarantine-denied tools.
pub const QUARANTINE_DENIED: &[&str] = &[
    // Shell execution
    "bash",
    // File write/mutation tools (FileExecutor IDs)
    "write",
    "edit",
    "delete_path",
    "move_path",
    "copy_path",
    "create_directory",
    // Web access
    "web_scrape",
    "fetch",
    // Runs `cargo check`/`cargo clippy`, which executes arbitrary code via build.rs
    // scripts and proc-macros in the target workspace — equivalent to `bash` for
    // security purposes.
    "diagnostics",
    // Memory persistence
    "memory_save",
    // Skill body retrieval — denied for Quarantined active skills to prevent
    // side-channel injection via dynamically loaded skill bodies.
    "load_skill",
    "invoke_skill",
];

/// Returns `true` if `tool_id` matches an entry in [`QUARANTINE_DENIED`], either exactly or
/// as an MCP-wrapped suffix (e.g. `filesystem_write` matches `write`, but
/// `filesystem_write_file` does not — suffix matching requires a `_` boundary immediately
/// before the denied entry).
///
/// Canonical predicate for `QUARANTINE_DENIED` membership — shared by `zeph-tools`
/// (`trust_gate::is_quarantine_denied`, re-exported from here) and `zeph-orchestration`,
/// which uses it alongside `tool_classification::is_readonly_tool` to close the blind spot
/// where a tool is read-only for autonomy-gating purposes (`READONLY_TOOLS`) yet still
/// denied under quarantine (`web_scrape`, `fetch`, `load_skill`, `invoke_skill` are in both
/// lists — see #6397).
#[must_use]
pub fn is_quarantine_denied(tool_id: &str) -> bool {
    QUARANTINE_DENIED
        .iter()
        .any(|denied| tool_id == *denied || tool_id.ends_with(&format!("_{denied}")))
}
