// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure-data security types for MCP tool metadata.
//!
//! These types are config-level data shapes — they carry no runtime logic and have no
//! dependency on `zeph-mcp` or any other feature crate. `zeph-mcp` re-exports them so
//! existing paths (`zeph_mcp::tool::ToolSecurityMeta`) continue to resolve.

use serde::{Deserialize, Serialize};

/// Sensitivity level of the data a tool accesses or produces.
///
/// Used by the data-flow policy to enforce that high-sensitivity tools can only be
/// registered on trusted servers. The ordering `None < Low < Medium < High` allows
/// `max()` comparisons when computing the worst-case sensitivity of a tool set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DataSensitivity {
    /// No sensitive data.
    #[default]
    None,
    /// Low-sensitivity data (e.g. public reads).
    Low,
    /// Medium-sensitivity data (e.g. internal reads, database queries).
    Medium,
    /// High-sensitivity data (e.g. writes, shell execution, credentials).
    High,
}

/// Coarse capability class for an MCP tool.
///
/// Assigned by operator config or inferred via heuristics at registration time.
/// Stored inside [`ToolSecurityMeta::capabilities`] and used by the data-flow policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityClass {
    /// Reads from the local filesystem.
    FilesystemRead,
    /// Writes to the local filesystem.
    FilesystemWrite,
    /// Makes outbound network calls.
    Network,
    /// Executes shell commands.
    Shell,
    /// Reads from a database.
    DatabaseRead,
    /// Writes to a database.
    DatabaseWrite,
    /// Writes to agent memory.
    MemoryWrite,
    /// Calls an external API.
    ExternalApi,
}

/// A parameter path and the injection pattern that matched it.
///
/// JSON pointer format: `/properties/key/description`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlaggedParameter {
    /// JSON pointer into `input_schema` identifying the flagged value.
    pub path: String,
    /// Name of the injection pattern that matched.
    pub pattern_name: String,
}

/// Per-tool security metadata.
///
/// Assigned by operator config or inferred from tool name heuristics at registration time.
/// Stored alongside `McpTool` in the tool registry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolSecurityMeta {
    /// Data sensitivity of this tool's outputs.
    #[serde(default)]
    pub data_sensitivity: DataSensitivity,
    /// Capability classes this tool exercises.
    #[serde(default)]
    pub capabilities: Vec<CapabilityClass>,
    /// Parameters whose `input_schema` values matched an injection pattern.
    #[serde(default)]
    pub flagged_parameters: Vec<FlaggedParameter>,
}
