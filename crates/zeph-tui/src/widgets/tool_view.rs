// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tool-view primitives: [`ToolKind`], [`ToolStatus`], and the density matrix.
//!
//! ## Density matrix
//!
//! Every `(ToolKind, ToolDensity)` pair maps to a single render mode:
//!
//! | `ToolDensity`  | All `ToolKind` variants                                    |
//! |----------------|------------------------------------------------------------|
//! | `Compact`      | Collapsed to a one-line group summary (no output body)     |
//! | `Inline`       | Minimal view: header + truncated output with ellipsis      |
//! | `Block`        | Fully expanded: header + full output body                  |
//!
//! The mapping is applied uniformly in [`ToolKind::is_groupable`]: every kind
//! participates in grouping so that density controls have a consistent effect
//! regardless of which tool was called.

/// Re-export of [`zeph_config::ToolDensity`] for use within the TUI widget layer.
///
/// Consumers within `zeph-tui` should import this re-export rather than
/// reaching into `zeph-config` directly so that the dependency is consistent.
pub use zeph_config::ToolDensity;

/// Category of a tool call, derived from the tool name.
///
/// Used to determine how consecutive tool messages are grouped in the chat
/// view and what verb is shown in the summary line.
///
/// # Examples
///
/// ```rust
/// use zeph_tui::widgets::tool_view::ToolKind;
///
/// assert_eq!(ToolKind::classify("bash", false), ToolKind::Run);
/// assert_eq!(ToolKind::classify("read_file", false), ToolKind::Explore);
/// assert_eq!(ToolKind::classify("write_file", false), ToolKind::Edit);
/// assert_eq!(ToolKind::classify("web_search", false), ToolKind::Web);
/// assert_eq!(ToolKind::classify("github_list_prs", true), ToolKind::Mcp);
/// assert_eq!(ToolKind::classify("unknown_tool", false), ToolKind::Other);
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// Shell / command execution tools (`bash`, `shell`, `run_command`).
    Run,
    /// Read-only filesystem inspection tools (`read_file`, `list_dir`, `grep`, `glob`).
    Explore,
    /// Filesystem write and patch tools (`write_file`, `edit_file`, `patch`).
    Edit,
    /// Web browsing and search tools (`web_search`, `web_scrape`, `fetch`).
    Web,
    /// Tools registered by an MCP server (`is_mcp` was `true`).
    Mcp,
    /// Any tool that does not match the above categories.
    Other,
}

impl ToolKind {
    /// Classify a tool by its canonical name and MCP origin.
    ///
    /// `is_mcp` must be resolved by the caller — real MCP tool ids are `{server_id}_{name}`
    /// (`McpTool::sanitized_id`) and carry no reliable string prefix to pattern-match on
    /// (#5712, #5734), so `tool_name` alone can never distinguish an MCP tool from a
    /// similarly-shaped built-in one. Callers with access to the tool's `ToolDef` should pass
    /// `ToolDef::is_mcp_tool()`; callers without it (e.g. no wiring yet from the event that
    /// produced `tool_name`) should pass `false` rather than guess.
    ///
    /// Name matching for the non-MCP categories is case-sensitive.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tui::widgets::tool_view::ToolKind;
    ///
    /// assert_eq!(ToolKind::classify("bash", false), ToolKind::Run);
    /// assert_eq!(ToolKind::classify("read_file", false), ToolKind::Explore);
    /// ```
    #[must_use]
    pub fn classify(tool_name: &str, is_mcp: bool) -> Self {
        if is_mcp {
            return Self::Mcp;
        }
        match tool_name {
            "bash" | "shell" | "run_command" | "run_shell_command" => Self::Run,
            "read_file" | "list_dir" | "glob" | "grep" | "find" | "ls" | "Read" | "list_files" => {
                Self::Explore
            }
            "write_file" | "edit_file" | "patch" | "Write" | "Edit" => Self::Edit,
            "web_search" | "web_scrape" | "fetch" | "WebSearch" | "WebFetch" => Self::Web,
            _ => Self::Other,
        }
    }

    /// Returns `true` when consecutive tool messages of the same kind can be
    /// collapsed into a group summary.
    ///
    /// All `ToolKind` variants are groupable so that [`ToolDensity`] takes
    /// uniform effect: `Compact` collapses every kind to a one-liner, `Inline`
    /// shows a truncated summary, and `Block` expands fully — regardless of
    /// whether the tool was a shell command, an edit, a web fetch, or an MCP
    /// call. Nameless tool messages (classified as [`ToolKind::Other`]) are
    /// excluded because they lack sufficient context for a meaningful summary.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tui::widgets::tool_view::ToolKind;
    ///
    /// assert!(ToolKind::Explore.is_groupable());
    /// assert!(ToolKind::Run.is_groupable());
    /// assert!(ToolKind::Edit.is_groupable());
    /// assert!(ToolKind::Web.is_groupable());
    /// assert!(ToolKind::Mcp.is_groupable());
    /// assert!(!ToolKind::Other.is_groupable());
    /// ```
    #[must_use]
    pub fn is_groupable(self) -> bool {
        !matches!(self, Self::Other)
    }

    /// Short display label for the kind, shown in group summary lines.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tui::widgets::tool_view::ToolKind;
    ///
    /// assert_eq!(ToolKind::Run.label(), "run");
    /// assert_eq!(ToolKind::Explore.label(), "explore");
    /// ```
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Explore => "explore",
            Self::Edit => "edit",
            Self::Web => "web",
            Self::Mcp => "mcp",
            Self::Other => "tool",
        }
    }
}

/// Visual status for a completed (or in-progress) tool call.
///
/// Determines which bullet character and colour are used in the chat view.
///
/// # Examples
///
/// ```rust
/// use zeph_tui::widgets::tool_view::ToolStatus;
///
/// let s = ToolStatus::from_streaming_and_success(false, Some(true));
/// assert_eq!(s, ToolStatus::Success);
///
/// let s = ToolStatus::from_streaming_and_success(true, None);
/// assert_eq!(s, ToolStatus::Running);
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    /// Tool is currently executing (spinner visible).
    Running,
    /// Tool completed successfully.
    Success,
    /// Tool completed with an error.
    Failure,
}

impl ToolStatus {
    /// Derive status from the streaming flag and optional success field.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tui::widgets::tool_view::ToolStatus;
    ///
    /// assert_eq!(ToolStatus::from_streaming_and_success(true, None), ToolStatus::Running);
    /// assert_eq!(ToolStatus::from_streaming_and_success(false, Some(true)), ToolStatus::Success);
    /// assert_eq!(ToolStatus::from_streaming_and_success(false, Some(false)), ToolStatus::Failure);
    /// assert_eq!(ToolStatus::from_streaming_and_success(false, None), ToolStatus::Success);
    /// ```
    #[must_use]
    pub fn from_streaming_and_success(streaming: bool, success: Option<bool>) -> Self {
        if streaming {
            Self::Running
        } else {
            match success {
                Some(false) => Self::Failure,
                _ => Self::Success,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_kind_classify_run() {
        assert_eq!(ToolKind::classify("bash", false), ToolKind::Run);
        assert_eq!(ToolKind::classify("shell", false), ToolKind::Run);
        assert_eq!(ToolKind::classify("run_command", false), ToolKind::Run);
    }

    #[test]
    fn tool_kind_classify_explore() {
        assert_eq!(ToolKind::classify("read_file", false), ToolKind::Explore);
        assert_eq!(ToolKind::classify("list_dir", false), ToolKind::Explore);
        assert_eq!(ToolKind::classify("grep", false), ToolKind::Explore);
        assert_eq!(ToolKind::classify("glob", false), ToolKind::Explore);
    }

    #[test]
    fn tool_kind_classify_edit() {
        assert_eq!(ToolKind::classify("write_file", false), ToolKind::Edit);
        assert_eq!(ToolKind::classify("edit_file", false), ToolKind::Edit);
        assert_eq!(ToolKind::classify("patch", false), ToolKind::Edit);
    }

    #[test]
    fn tool_kind_classify_web() {
        assert_eq!(ToolKind::classify("web_search", false), ToolKind::Web);
        assert_eq!(ToolKind::classify("web_scrape", false), ToolKind::Web);
        assert_eq!(ToolKind::classify("fetch", false), ToolKind::Web);
    }

    /// #5734 regression: MCP origin must come from the caller-supplied `is_mcp` flag (ultimately
    /// backed by `ToolDef::is_mcp_tool()`), not a `"mcp__"` string prefix — real MCP tool ids are
    /// `{server_id}_{name}` (`McpTool::sanitized_id`) and never carry that prefix, so the old
    /// check silently never matched any real MCP tool.
    #[test]
    fn tool_kind_classify_mcp() {
        assert_eq!(ToolKind::classify("github_list_prs", true), ToolKind::Mcp);
        assert_eq!(ToolKind::classify("slack_send", true), ToolKind::Mcp);
        // A real-world-shaped MCP id without the caller-supplied flag is not misclassified.
        assert_eq!(
            ToolKind::classify("github_list_prs", false),
            ToolKind::Other
        );
    }

    #[test]
    fn tool_kind_classify_other() {
        assert_eq!(ToolKind::classify("unknown_tool", false), ToolKind::Other);
        assert_eq!(ToolKind::classify("memory_search", false), ToolKind::Other);
    }

    #[test]
    fn tool_kind_groupable() {
        assert!(ToolKind::Run.is_groupable());
        assert!(ToolKind::Explore.is_groupable());
        assert!(ToolKind::Edit.is_groupable());
        assert!(ToolKind::Web.is_groupable());
        assert!(ToolKind::Mcp.is_groupable());
        assert!(!ToolKind::Other.is_groupable());
    }

    #[test]
    fn tool_density_cycle() {
        assert_eq!(ToolDensity::Compact.cycle(), ToolDensity::Inline);
        assert_eq!(ToolDensity::Inline.cycle(), ToolDensity::Block);
        assert_eq!(ToolDensity::Block.cycle(), ToolDensity::Compact);
    }

    #[test]
    fn tool_density_default_is_inline() {
        assert_eq!(ToolDensity::default(), ToolDensity::Inline);
    }

    #[test]
    fn tool_status_from_streaming_and_success() {
        assert_eq!(
            ToolStatus::from_streaming_and_success(true, None),
            ToolStatus::Running
        );
        assert_eq!(
            ToolStatus::from_streaming_and_success(false, Some(true)),
            ToolStatus::Success
        );
        assert_eq!(
            ToolStatus::from_streaming_and_success(false, Some(false)),
            ToolStatus::Failure
        );
        assert_eq!(
            ToolStatus::from_streaming_and_success(false, None),
            ToolStatus::Success
        );
    }
}
