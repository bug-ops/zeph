// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::subagent::{HookDef, MemoryScope, PermissionMode};

fn default_max_tool_iterations() -> usize {
    10
}

fn default_auto_update_check() -> bool {
    true
}

fn default_focus_compression_interval() -> usize {
    12
}

fn default_focus_reminder_interval() -> usize {
    15
}

fn default_focus_min_messages_per_focus() -> usize {
    8
}

fn default_focus_max_knowledge_tokens() -> usize {
    4096
}

fn default_max_tool_retries() -> usize {
    2
}

fn default_max_retry_duration_secs() -> u64 {
    30
}

fn default_tool_repeat_threshold() -> usize {
    2
}

fn default_tool_filter_top_k() -> usize {
    6
}

fn default_tool_filter_min_description_words() -> usize {
    5
}

fn default_tool_filter_always_on() -> Vec<String> {
    vec![
        "memory_search".into(),
        "memory_save".into(),
        "load_skill".into(),
        "bash".into(),
        "read".into(),
        "edit".into(),
    ]
}

fn default_instruction_auto_detect() -> bool {
    true
}

fn default_max_concurrent() -> usize {
    5
}

fn default_transcript_enabled() -> bool {
    true
}

fn default_transcript_max_files() -> usize {
    50
}

/// Configuration for focus-based active context compression (#1850).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct FocusConfig {
    /// Enable focus tools (`start_focus` / `complete_focus`). Default: `false`.
    pub enabled: bool,
    /// Suggest focus after this many turns without one. Default: `12`.
    #[serde(default = "default_focus_compression_interval")]
    pub compression_interval: usize,
    /// Remind the agent every N turns when focus is overdue. Default: `15`.
    #[serde(default = "default_focus_reminder_interval")]
    pub reminder_interval: usize,
    /// Minimum messages required before suggesting a focus. Default: `8`.
    #[serde(default = "default_focus_min_messages_per_focus")]
    pub min_messages_per_focus: usize,
    /// Maximum tokens the Knowledge block may grow to before old entries are trimmed.
    /// Default: `4096`.
    #[serde(default = "default_focus_max_knowledge_tokens")]
    pub max_knowledge_tokens: usize,
}

impl Default for FocusConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            compression_interval: default_focus_compression_interval(),
            reminder_interval: default_focus_reminder_interval(),
            min_messages_per_focus: default_focus_min_messages_per_focus(),
            max_knowledge_tokens: default_focus_max_knowledge_tokens(),
        }
    }
}

/// Dynamic tool schema filtering configuration (#2020).
///
/// When enabled, only a subset of tool definitions is sent to the LLM on each turn,
/// selected by embedding similarity between the user query and tool descriptions.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ToolFilterConfig {
    /// Enable dynamic tool schema filtering. Default: `false` (opt-in).
    pub enabled: bool,
    /// Number of top-scoring filterable tools to include per turn.
    /// Set to `0` to include all filterable tools.
    #[serde(default = "default_tool_filter_top_k")]
    pub top_k: usize,
    /// Tool IDs that are never filtered out.
    #[serde(default = "default_tool_filter_always_on")]
    pub always_on: Vec<String>,
    /// MCP tools with fewer description words than this are auto-included.
    #[serde(default = "default_tool_filter_min_description_words")]
    pub min_description_words: usize,
}

impl Default for ToolFilterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            top_k: default_tool_filter_top_k(),
            always_on: default_tool_filter_always_on(),
            min_description_words: default_tool_filter_min_description_words(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AgentConfig {
    pub name: String,
    #[serde(default = "default_max_tool_iterations")]
    pub max_tool_iterations: usize,
    #[serde(default = "default_auto_update_check")]
    pub auto_update_check: bool,
    /// Additional instruction files to always load, regardless of provider.
    #[serde(default)]
    pub instruction_files: Vec<std::path::PathBuf>,
    /// When true, automatically detect provider-specific instruction files
    /// (e.g. `CLAUDE.md` for Claude, `AGENTS.md` for `OpenAI`).
    #[serde(default = "default_instruction_auto_detect")]
    pub instruction_auto_detect: bool,
    /// Maximum retry attempts for transient tool errors (0 to disable).
    #[serde(default = "default_max_tool_retries")]
    pub max_tool_retries: usize,
    /// Number of identical tool+args calls within the recent window to trigger repeat-detection
    /// abort (0 to disable).
    #[serde(default = "default_tool_repeat_threshold")]
    pub tool_repeat_threshold: usize,
    /// Maximum total wall-clock time (seconds) to spend on retries for a single tool call.
    #[serde(default = "default_max_retry_duration_secs")]
    pub max_retry_duration_secs: u64,
    /// Focus-based active context compression configuration (#1850).
    #[serde(default)]
    pub focus: FocusConfig,
    /// Dynamic tool schema filtering configuration (#2020).
    #[serde(default)]
    pub tool_filter: ToolFilterConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SubAgentConfig {
    pub enabled: bool,
    /// Maximum number of sub-agents that can run concurrently.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    pub extra_dirs: Vec<PathBuf>,
    /// User-level agents directory.
    #[serde(default)]
    pub user_agents_dir: Option<PathBuf>,
    /// Default permission mode applied to sub-agents that do not specify one.
    pub default_permission_mode: Option<PermissionMode>,
    /// Global denylist applied to all sub-agents in addition to per-agent `tools.except`.
    #[serde(default)]
    pub default_disallowed_tools: Vec<String>,
    /// Allow sub-agents to use `bypass_permissions` mode.
    #[serde(default)]
    pub allow_bypass_permissions: bool,
    /// Default memory scope applied to sub-agents that do not set `memory` in their definition.
    #[serde(default)]
    pub default_memory_scope: Option<MemoryScope>,
    /// Lifecycle hooks executed when any sub-agent starts or stops.
    #[serde(default)]
    pub hooks: SubAgentLifecycleHooks,
    /// Directory where transcript JSONL files and meta sidecars are stored.
    #[serde(default)]
    pub transcript_dir: Option<PathBuf>,
    /// Enable writing JSONL transcripts for sub-agent sessions.
    #[serde(default = "default_transcript_enabled")]
    pub transcript_enabled: bool,
    /// Maximum number of `.jsonl` transcript files to keep.
    #[serde(default = "default_transcript_max_files")]
    pub transcript_max_files: usize,
}

impl Default for SubAgentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_concurrent: default_max_concurrent(),
            extra_dirs: Vec::new(),
            user_agents_dir: None,
            default_permission_mode: None,
            default_disallowed_tools: Vec::new(),
            allow_bypass_permissions: false,
            default_memory_scope: None,
            hooks: SubAgentLifecycleHooks::default(),
            transcript_dir: None,
            transcript_enabled: default_transcript_enabled(),
            transcript_max_files: default_transcript_max_files(),
        }
    }
}

/// Config-level lifecycle hooks fired when any sub-agent starts or stops.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SubAgentLifecycleHooks {
    /// Hooks run after a sub-agent is spawned (fire-and-forget).
    pub start: Vec<HookDef>,
    /// Hooks run after a sub-agent finishes or is cancelled (fire-and-forget).
    pub stop: Vec<HookDef>,
}
