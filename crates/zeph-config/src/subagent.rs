// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

// ── PermissionMode ─────────────────────────────────────────────────────────

/// Controls tool execution and prompt interactivity for a sub-agent.
///
/// For sub-agents (non-interactive), `Default`, `AcceptEdits`, `DontAsk`, and
/// `BypassPermissions` are functionally equivalent — sub-agents never prompt the
/// user. The meaningful differentiator is `Plan` mode, which suppresses all tool
/// execution and returns only the plan text.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Standard behavior — prompt for each action (sub-agents auto-approve).
    #[default]
    Default,
    /// Auto-accept file edits without prompting.
    AcceptEdits,
    /// Auto-approve all tool calls without prompting.
    DontAsk,
    /// Unrestricted tool access; emits a warning when loaded.
    BypassPermissions,
    /// Read-only planning: tools are visible in the catalog but execution is blocked.
    Plan,
}

// ── MemoryScope ────────────────────────────────────────────────────────────

/// Persistence scope for sub-agent memory files.
///
/// Determines where the agent's `MEMORY.md` and topic files are stored across sessions.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    /// User-level: `~/.zeph/agent-memory/<name>/`.
    User,
    /// Project-level: `.zeph/agent-memory/<name>/`.
    Project,
    /// Local-only: `.zeph/agent-memory-local/<name>/`.
    Local,
}

// ── ToolPolicy ─────────────────────────────────────────────────────────────

/// Tool access policy for a sub-agent.
///
/// Controls which tools the sub-agent may call, independent of the global tool denylist.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPolicy {
    /// Only the listed tool IDs are accessible.
    AllowList(Vec<String>),
    /// All tools except those in the list are accessible.
    DenyList(Vec<String>),
    /// Inherit the full tool set from the parent agent (no additional filtering).
    InheritAll,
}

// ── SkillFilter ────────────────────────────────────────────────────────────

/// Skill allow/deny filter for sub-agent definitions.
///
/// Skills named in `include` are the only ones loaded; `exclude` removes
/// specific skills from the inherited set. When both are empty the sub-agent
/// inherits all parent skills.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillFilter {
    /// Explicit skill names to include (empty = inherit all).
    pub include: Vec<String>,
    /// Skill names to remove from the inherited set.
    pub exclude: Vec<String>,
}

impl SkillFilter {
    /// Returns `true` when no filter is applied (all skills are inherited).
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_config::SkillFilter;
    ///
    /// assert!(SkillFilter::default().is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }
}

// ── HookDef / HookType / HookMatcher / SubagentHooks ──────────────────────

/// The type of hook — currently only shell command hooks are supported.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HookType {
    Command,
}

fn default_hook_timeout() -> u64 {
    30
}

/// A single hook definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookDef {
    #[serde(rename = "type")]
    pub hook_type: HookType,
    pub command: String,
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u64,
    /// When `true`, a non-zero exit code or timeout causes the calling operation to fail.
    /// When `false` (default), errors are logged but execution continues.
    #[serde(default)]
    pub fail_closed: bool,
}

/// Tool-name matcher with associated hooks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookMatcher {
    pub matcher: String,
    pub hooks: Vec<HookDef>,
}

/// Per-agent frontmatter hook collections (`PreToolUse` / `PostToolUse`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SubagentHooks {
    #[serde(default)]
    pub pre_tool_use: Vec<HookMatcher>,
    #[serde(default)]
    pub post_tool_use: Vec<HookMatcher>,
}

impl SubagentHooks {
    /// Returns `true` when no pre- or post-tool-use hooks are configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pre_tool_use.is_empty() && self.post_tool_use.is_empty()
    }
}
