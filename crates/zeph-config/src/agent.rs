// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::subagent::{HookDef, MemoryScope, PermissionMode};

/// Specifies which LLM provider a sub-agent should use.
///
/// Used in `SubAgentDef.model` frontmatter field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSpec {
    /// Use the parent agent's active provider at spawn time.
    Inherit,
    /// Use a specific named provider from `[[llm.providers]]`.
    Named(String),
}

impl ModelSpec {
    /// Return the string representation: `"inherit"` or the provider name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            ModelSpec::Inherit => "inherit",
            ModelSpec::Named(s) => s.as_str(),
        }
    }
}

impl Serialize for ModelSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            ModelSpec::Inherit => serializer.serialize_str("inherit"),
            ModelSpec::Named(s) => serializer.serialize_str(s),
        }
    }
}

impl<'de> Deserialize<'de> for ModelSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s == "inherit" {
            Ok(ModelSpec::Inherit)
        } else {
            Ok(ModelSpec::Named(s))
        }
    }
}

/// Controls how parent agent context is injected into a spawned sub-agent's task prompt.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextInjectionMode {
    /// No parent context injected.
    None,
    /// Prepend the last assistant turn from parent history as a preamble.
    #[default]
    LastAssistantTurn,
    /// LLM-generated summary of parent context (not yet implemented in Phase 1).
    Summary,
}

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

fn default_focus_auto_consolidate_min_window() -> usize {
    6
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
        "invoke_skill".into(),
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

fn default_context_window_turns() -> usize {
    10
}

fn default_max_spawn_depth() -> u32 {
    3
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
    /// Minimum messages in a low-relevance window before auto-consolidation runs
    /// when `[memory.compression]` strategy is `focus` (#3313).
    ///
    /// Distinct from `min_messages_per_focus` (which gates manual focus sessions).
    /// Default: `6`.
    #[serde(default = "default_focus_auto_consolidate_min_window")]
    pub auto_consolidate_min_window: usize,
}

impl Default for FocusConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            compression_interval: default_focus_compression_interval(),
            reminder_interval: default_focus_reminder_interval(),
            min_messages_per_focus: default_focus_min_messages_per_focus(),
            max_knowledge_tokens: default_focus_max_knowledge_tokens(),
            auto_consolidate_min_window: default_focus_auto_consolidate_min_window(),
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

/// Core agent behavior configuration, nested under `[agent]` in TOML.
///
/// Controls the agent's name, tool-loop limits, instruction loading, and retry
/// behavior. All fields have sensible defaults; only `name` is typically changed
/// by end users.
///
/// # Example (TOML)
///
/// ```toml
/// [agent]
/// name = "Zeph"
/// max_tool_iterations = 15
/// max_tool_retries = 3
/// ```
#[derive(Debug, Deserialize, Serialize)]
pub struct AgentConfig {
    /// Human-readable agent name surfaced in the TUI and Telegram header. Default: `"Zeph"`.
    pub name: String,
    /// Maximum number of tool-call iterations per agent turn before the loop is aborted.
    /// Must be `<= 100`. Default: `10`.
    #[serde(default = "default_max_tool_iterations")]
    pub max_tool_iterations: usize,
    /// Check for new Zeph releases at startup. Default: `true`.
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
    /// Inject a `<budget>` XML block into the volatile system prompt section so the LLM
    /// can self-regulate tool calls and cost. Self-suppresses when no budget data is
    /// available (#2267).
    #[serde(default = "default_budget_hint_enabled")]
    pub budget_hint_enabled: bool,
    /// Background task supervisor tuning. Controls concurrency limits and turn-boundary abort.
    #[serde(default)]
    pub supervisor: TaskSupervisorConfig,
}

fn default_budget_hint_enabled() -> bool {
    true
}

fn default_enrichment_limit() -> usize {
    4
}

fn default_telemetry_limit() -> usize {
    8
}

fn default_background_shell_limit() -> usize {
    8
}

/// Background task supervisor configuration, nested under `[agent.supervisor]` in TOML.
///
/// Controls per-class concurrency limits and turn-boundary behaviour for the
/// `BackgroundSupervisor` in `zeph-core`.
/// All fields have sensible defaults that match the Phase 1 hardcoded values; only change
/// these if you observe excessive background task drops under load.
///
/// # Example (TOML)
///
/// ```toml
/// [agent.supervisor]
/// enrichment_limit = 4
/// telemetry_limit = 8
/// abort_enrichment_on_turn = false
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TaskSupervisorConfig {
    /// Maximum concurrent enrichment tasks (summarization, graph/persona/trajectory extraction).
    /// Default: `4`.
    #[serde(default = "default_enrichment_limit")]
    pub enrichment_limit: usize,
    /// Maximum concurrent telemetry tasks (audit log writes, graph count sync).
    /// Default: `8`.
    #[serde(default = "default_telemetry_limit")]
    pub telemetry_limit: usize,
    /// Abort all inflight enrichment tasks at turn boundary to prevent backlog buildup.
    /// Default: `false`.
    #[serde(default)]
    pub abort_enrichment_on_turn: bool,
    /// Maximum concurrent background shell runs tracked by the supervisor.
    ///
    /// Should match `tools.shell.max_background_runs` so both layers agree on capacity.
    /// Default: `8`.
    #[serde(default = "default_background_shell_limit")]
    pub background_shell_limit: usize,
}

impl Default for TaskSupervisorConfig {
    fn default() -> Self {
        Self {
            enrichment_limit: default_enrichment_limit(),
            telemetry_limit: default_telemetry_limit(),
            abort_enrichment_on_turn: false,
            background_shell_limit: default_background_shell_limit(),
        }
    }
}

/// Sub-agent pool configuration, nested under `[agents]` in TOML.
///
/// When `enabled = true`, the agent can spawn isolated sub-agent sessions from
/// SKILL.md-based agent definitions. Sub-agents inherit the parent's provider pool
/// unless overridden by `model` in their definition frontmatter.
///
/// # Example (TOML)
///
/// ```toml
/// [agents]
/// enabled = true
/// max_concurrent = 3
/// max_spawn_depth = 2
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SubAgentConfig {
    /// Enable the sub-agent subsystem. Default: `false`.
    pub enabled: bool,
    /// Maximum number of sub-agents that can run concurrently.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// Additional directories to search for `.agent.md` definition files.
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
    /// Number of recent parent conversation turns to pass to spawned sub-agents.
    /// Set to 0 to disable history propagation.
    #[serde(default = "default_context_window_turns")]
    pub context_window_turns: usize,
    /// Maximum nesting depth for sub-agent spawns.
    #[serde(default = "default_max_spawn_depth")]
    pub max_spawn_depth: u32,
    /// How parent context is injected into the sub-agent's task prompt.
    #[serde(default)]
    pub context_injection_mode: ContextInjectionMode,
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
            context_window_turns: default_context_window_turns(),
            max_spawn_depth: default_max_spawn_depth(),
            context_injection_mode: ContextInjectionMode::default(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_config_defaults() {
        let cfg = SubAgentConfig::default();
        assert_eq!(cfg.context_window_turns, 10);
        assert_eq!(cfg.max_spawn_depth, 3);
        assert_eq!(
            cfg.context_injection_mode,
            ContextInjectionMode::LastAssistantTurn
        );
    }

    #[test]
    fn subagent_config_deserialize_new_fields() {
        let toml_str = r#"
            enabled = true
            context_window_turns = 5
            max_spawn_depth = 2
            context_injection_mode = "none"
        "#;
        let cfg: SubAgentConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.context_window_turns, 5);
        assert_eq!(cfg.max_spawn_depth, 2);
        assert_eq!(cfg.context_injection_mode, ContextInjectionMode::None);
    }

    #[test]
    fn model_spec_deserialize_inherit() {
        let spec: ModelSpec = serde_json::from_str("\"inherit\"").unwrap();
        assert_eq!(spec, ModelSpec::Inherit);
    }

    #[test]
    fn model_spec_deserialize_named() {
        let spec: ModelSpec = serde_json::from_str("\"fast\"").unwrap();
        assert_eq!(spec, ModelSpec::Named("fast".to_owned()));
    }

    #[test]
    fn model_spec_as_str() {
        assert_eq!(ModelSpec::Inherit.as_str(), "inherit");
        assert_eq!(ModelSpec::Named("x".to_owned()).as_str(), "x");
    }

    #[test]
    fn focus_config_auto_consolidate_min_window_default_is_six() {
        let cfg = FocusConfig::default();
        assert_eq!(cfg.auto_consolidate_min_window, 6);
    }

    #[test]
    fn focus_config_auto_consolidate_min_window_deserializes() {
        let toml_str = "auto_consolidate_min_window = 10";
        let cfg: FocusConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.auto_consolidate_min_window, 10);
    }
}
