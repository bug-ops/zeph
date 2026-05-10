// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

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
/// assert_eq!(ToolKind::classify("bash"), ToolKind::Run);
/// assert_eq!(ToolKind::classify("read_file"), ToolKind::Explore);
/// assert_eq!(ToolKind::classify("write_file"), ToolKind::Edit);
/// assert_eq!(ToolKind::classify("web_search"), ToolKind::Web);
/// assert_eq!(ToolKind::classify("mcp__github__list_prs"), ToolKind::Mcp);
/// assert_eq!(ToolKind::classify("unknown_tool"), ToolKind::Other);
/// ```
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
    /// MCP-namespaced tools (name starts with `mcp__`).
    Mcp,
    /// Any tool that does not match the above categories.
    Other,
}

impl ToolKind {
    /// Classify a tool by its canonical name.
    ///
    /// Matching is case-sensitive and prefix-based for MCP tools.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tui::widgets::tool_view::ToolKind;
    ///
    /// assert_eq!(ToolKind::classify("bash"), ToolKind::Run);
    /// assert_eq!(ToolKind::classify("read_file"), ToolKind::Explore);
    /// ```
    #[must_use]
    pub fn classify(tool_name: &str) -> Self {
        if tool_name.starts_with("mcp__") {
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

    /// Returns `true` for kinds that can be visually grouped when consecutive.
    ///
    /// Only `Run` and `Explore` tools are grouped because they tend to appear
    /// in repetitive sequences (e.g. 10 `read_file` calls). Edit, Web, and MCP
    /// tools carry distinct content that should remain individually visible.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tui::widgets::tool_view::ToolKind;
    ///
    /// assert!(ToolKind::Explore.is_groupable());
    /// assert!(ToolKind::Run.is_groupable());
    /// assert!(!ToolKind::Edit.is_groupable());
    /// ```
    #[must_use]
    pub fn is_groupable(self) -> bool {
        matches!(self, Self::Run | Self::Explore)
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
        assert_eq!(ToolKind::classify("bash"), ToolKind::Run);
        assert_eq!(ToolKind::classify("shell"), ToolKind::Run);
        assert_eq!(ToolKind::classify("run_command"), ToolKind::Run);
    }

    #[test]
    fn tool_kind_classify_explore() {
        assert_eq!(ToolKind::classify("read_file"), ToolKind::Explore);
        assert_eq!(ToolKind::classify("list_dir"), ToolKind::Explore);
        assert_eq!(ToolKind::classify("grep"), ToolKind::Explore);
        assert_eq!(ToolKind::classify("glob"), ToolKind::Explore);
    }

    #[test]
    fn tool_kind_classify_edit() {
        assert_eq!(ToolKind::classify("write_file"), ToolKind::Edit);
        assert_eq!(ToolKind::classify("edit_file"), ToolKind::Edit);
        assert_eq!(ToolKind::classify("patch"), ToolKind::Edit);
    }

    #[test]
    fn tool_kind_classify_web() {
        assert_eq!(ToolKind::classify("web_search"), ToolKind::Web);
        assert_eq!(ToolKind::classify("web_scrape"), ToolKind::Web);
        assert_eq!(ToolKind::classify("fetch"), ToolKind::Web);
    }

    #[test]
    fn tool_kind_classify_mcp() {
        assert_eq!(ToolKind::classify("mcp__github__list_prs"), ToolKind::Mcp);
        assert_eq!(ToolKind::classify("mcp__slack__send"), ToolKind::Mcp);
    }

    #[test]
    fn tool_kind_classify_other() {
        assert_eq!(ToolKind::classify("unknown_tool"), ToolKind::Other);
        assert_eq!(ToolKind::classify("memory_search"), ToolKind::Other);
    }

    #[test]
    fn tool_kind_groupable() {
        assert!(ToolKind::Run.is_groupable());
        assert!(ToolKind::Explore.is_groupable());
        assert!(!ToolKind::Edit.is_groupable());
        assert!(!ToolKind::Web.is_groupable());
        assert!(!ToolKind::Mcp.is_groupable());
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
