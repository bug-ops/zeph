// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Quarantine-denied tool list shared between `zeph-tools` and `zeph-skills`.
//!
//! [`QUARANTINE_DENIED`] is the single source of truth for tools that are blocked when a
//! skill operates at the [`crate::SkillTrustLevel::Quarantined`] level.

/// Tools denied when a Quarantined skill is active.
///
/// Uses the actual tool IDs registered by `FileExecutor` and other executors.
/// MCP tools use a server-prefixed ID (e.g. `filesystem_write_file`). The
/// `is_quarantine_denied` predicate in `zeph-tools` checks both exact matches and
/// `_{entry}` suffix matches to cover MCP-wrapped versions of these native tool IDs.
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
    // Memory persistence
    "memory_save",
    // Skill body retrieval — denied for Quarantined active skills to prevent
    // side-channel injection via dynamically loaded skill bodies.
    "load_skill",
    "invoke_skill",
];
