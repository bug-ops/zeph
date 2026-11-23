// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Canonical tool identity shared by the sub-agent filter, hook matchers and the router.
//!
//! Tool names reach the runtime in several spellings (`Bash`, `shell`, `sh`, `Bash(cargo *)`).
//! [`ToolMatchKey`] collapses them to the runtime tool id so every access-control and routing
//! decision compares the same value.

/// Spellings that all denote the shell executor (runtime id `bash`).
pub const SHELL_TOOL_ALIASES: [&str; 3] = ["bash", "sh", "shell"];

const SHELL_RUNTIME_ID: &str = "bash";

/// Canonical, comparison-only form of a tool name.
///
/// Construction trims whitespace, drops an argument pattern (everything from the first `(`),
/// lowercases, and resolves shell aliases to the runtime id `bash`. Keys are meant for equality
/// checks, never for glob matching.
///
/// # Examples
///
/// ```
/// use zeph_tools::tool_id::ToolMatchKey;
///
/// assert_eq!(ToolMatchKey::new("Shell"), ToolMatchKey::new("bash(cargo *)"));
/// assert_eq!(ToolMatchKey::new("Read").as_str(), "read");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolMatchKey(Box<str>);

impl ToolMatchKey {
    /// Builds the canonical key for `raw`. Total and idempotent.
    #[must_use]
    pub fn new(raw: &str) -> Self {
        let base = raw.split('(').next().unwrap_or(raw).trim().to_lowercase();
        if SHELL_TOOL_ALIASES.contains(&base.as_str()) {
            Self(SHELL_RUNTIME_ID.into())
        } else {
            Self(base.into_boxed_str())
        }
    }

    /// The canonical tool id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the key and returns the canonical tool id.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0.into_string()
    }
}

/// Returns the argument pattern of an entry such as `Bash(cargo *)`, if it has one.
///
/// Argument patterns are not enforced anywhere: the entry is reduced to its whole tool.
///
/// # Examples
///
/// ```
/// use zeph_tools::tool_id::argument_pattern;
///
/// assert_eq!(argument_pattern("Bash(cargo *)"), Some("cargo *"));
/// assert_eq!(argument_pattern("bash"), None);
/// ```
#[must_use]
pub fn argument_pattern(raw: &str) -> Option<&str> {
    let (_, rest) = raw.split_once('(')?;
    let rest = rest.trim_end();
    Some(rest.trim_suffix(')'))
}
