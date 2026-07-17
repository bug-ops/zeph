// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Read-only tool classification shared between `zeph-tools` and `zeph-orchestration`.
//!
//! [`READONLY_TOOLS`] is the single source of truth for native tool IDs that are
//! read-only (no filesystem/state mutation, no code execution). `zeph-tools` uses it to
//! gate `ReadOnly` autonomy mode and to bypass `Supervised`-mode confirmation for
//! unconfigured tools (see #5575). `zeph-orchestration` uses it to classify tool calls in
//! a task's real execution trace as read vs. write-type, so a mixed trace (successful
//! reads followed by policy-blocked writes) is not mistaken for genuine partial progress
//! (see #6397).

/// Read-only tool allowlist (available in `ReadOnly` autonomy mode).
///
/// Also used, via [`is_readonly_tool`], to bypass the `Supervised`-mode confirmation
/// default for unconfigured tools — see #5575. A tool added here is trusted to run
/// without confirmation in *both* modes; do not add anything that mutates state or
/// executes code.
pub const READONLY_TOOLS: &[&str] = &[
    "read",
    "find_path",
    "grep",
    "list_directory",
    "web_scrape",
    "fetch",
    "load_skill",
    "invoke_skill",
];

/// Returns `true` if `tool_id` is a native read-only tool.
///
/// Reuses the same [`READONLY_TOOLS`] allowlist that gates `ReadOnly` autonomy mode, so
/// `Supervised` mode's unconfigured-tool bypass (`TrustGateExecutor::check_trust`) cannot
/// silently diverge from it — see #5575.
#[must_use]
pub fn is_readonly_tool(tool_id: &str) -> bool {
    READONLY_TOOLS.contains(&tool_id)
}
