// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use zeph_llm::ThinkingConfig;
use zeph_skills::TrustLevel;
use zeph_tools::{AutonomyLevel, ToolsConfig};

use crate::sanitizer::ContentIsolationConfig;
use crate::sanitizer::exfiltration::ExfiltrationGuardConfig;

use crate::subagent::def::{MemoryScope, PermissionMode};
use crate::subagent::hooks::HookDef;

use crate::vault::Secret;

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    pub agent: AgentConfig,
    pub llm: LlmConfig,
    pub skills: SkillsConfig,
    pub memory: MemoryConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telegram: Option<TelegramConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discord: Option<DiscordConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slack: Option<SlackConfig>,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub a2a: A2aServerConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub index: IndexConfig,
    #[serde(default)]
    pub vault: VaultConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub timeouts: TimeoutConfig,
    #[serde(default)]
    pub cost: CostConfig,
    #[serde(default)]
    pub observability: ObservabilityConfig,
    #[serde(default)]
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    #[serde(default)]
    pub tui: TuiConfig,
    #[serde(default)]
    pub acp: AcpConfig,
    #[serde(default)]
    pub agents: SubAgentConfig,
    #[serde(default)]
    pub orchestration: OrchestrationConfig,
    #[serde(skip)]
    pub secrets: ResolvedSecrets,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SubAgentConfig {
    pub enabled: bool,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    pub extra_dirs: Vec<PathBuf>,
    /// User-level agents directory.
    ///
    /// Set to an absolute path to override the platform default (`~/.config/zeph/agents`
    /// on Linux/macOS, `%APPDATA%/zeph/agents` on Windows). Note: tilde (`~`) expansion
    /// is not supported — use an absolute path or omit this field to use the platform default.
    /// Set to empty string to disable the user-level directory entirely.
    #[serde(default)]
    pub user_agents_dir: Option<PathBuf>,
    /// Default permission mode applied to sub-agents that do not specify one.
    ///
    /// Only takes effect when the sub-agent definition leaves `permission_mode` at its
    /// default value (`Default`). If the definition sets an explicit mode, this field is
    /// ignored. `Some(PermissionMode::Default)` behaves identically to `None` — both
    /// result in `Default` mode. Prefer omitting the field over explicitly setting
    /// `default_permission_mode = "default"` in config.
    pub default_permission_mode: Option<PermissionMode>,
    /// Global denylist applied to all sub-agents in addition to per-agent `tools.except`.
    #[serde(default)]
    pub default_disallowed_tools: Vec<String>,
    /// Allow sub-agents to use `bypass_permissions` mode.
    ///
    /// When `false` (default), spawning a sub-agent with `permission_mode: bypass_permissions`
    /// is rejected with an error. Set to `true` only in trusted, controlled environments.
    #[serde(default)]
    pub allow_bypass_permissions: bool,
    /// Default memory scope applied to sub-agents that do not set `memory` in their definition.
    ///
    /// When set, all agents without an explicit `memory` field will use this scope.
    /// Set to `None` (omit from config) to disable memory by default.
    ///
    /// **Note**: Setting this affects ALL agents without an explicit `memory` field.
    /// Agents can opt out by setting `memory: ~` in their frontmatter (not yet supported — None
    /// means "not specified", which falls back to this default).
    #[serde(default)]
    pub default_memory_scope: Option<MemoryScope>,
    /// Lifecycle hooks executed when any sub-agent starts or stops.
    ///
    /// `start` hooks run after the agent is spawned (fire-and-forget).
    /// `stop` hooks run after the agent finishes or is cancelled (fire-and-forget).
    #[serde(default)]
    pub hooks: SubAgentLifecycleHooks,
    /// Directory where transcript JSONL files and meta sidecars are stored.
    ///
    /// Defaults to `.zeph/subagents` relative to the working directory when `None`.
    #[serde(default)]
    pub transcript_dir: Option<PathBuf>,
    /// Enable writing JSONL transcripts for sub-agent sessions.
    ///
    /// When `false`, no transcript files are written and `/agent resume` is unavailable.
    #[serde(default = "default_transcript_enabled")]
    pub transcript_enabled: bool,
    /// Maximum number of `.jsonl` transcript files to keep.
    ///
    /// When the count exceeds this limit, the oldest files are deleted on each spawn or
    /// resume. `0` means unlimited (no cleanup performed).
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

fn default_max_concurrent() -> usize {
    1
}

fn default_transcript_enabled() -> bool {
    true
}

fn default_transcript_max_files() -> usize {
    50
}

fn default_max_tool_iterations() -> usize {
    10
}

fn default_auto_update_check() -> bool {
    true
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AgentConfig {
    pub name: String,
    #[serde(default = "default_max_tool_iterations")]
    pub max_tool_iterations: usize,
    #[serde(default)]
    pub summary_model: Option<String>,
    #[serde(default = "default_auto_update_check")]
    pub auto_update_check: bool,
    /// Additional instruction files to always load, regardless of provider.
    #[serde(default)]
    pub instruction_files: Vec<std::path::PathBuf>,
    /// When true, automatically detect provider-specific instruction files
    /// (e.g. `CLAUDE.md` for Claude, `AGENTS.md` for `OpenAI`).
    #[serde(default = "default_instruction_auto_detect")]
    pub instruction_auto_detect: bool,
}

fn default_instruction_auto_detect() -> bool {
    true
}

/// LLM provider backend selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Ollama,
    Claude,
    OpenAi,
    Candle,
    Orchestrator,
    Compatible,
    Router,
}

impl ProviderKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::Claude => "claude",
            Self::OpenAi => "openai",
            Self::Candle => "candle",
            Self::Orchestrator => "orchestrator",
            Self::Compatible => "compatible",
            Self::Router => "router",
        }
    }
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct LlmConfig {
    pub provider: ProviderKind,
    pub base_url: String,
    pub model: String,
    #[serde(default = "default_embedding_model")]
    pub embedding_model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud: Option<CloudLlmConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub openai: Option<OpenAiConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candle: Option<CandleConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orchestrator: Option<OrchestratorConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatible: Option<Vec<CompatibleConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub router: Option<RouterConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ollama: Option<OllamaConfig>,
    pub stt: Option<SttConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision_model: Option<String>,
    #[serde(default)]
    pub response_cache_enabled: bool,
    #[serde(default = "default_response_cache_ttl_secs")]
    pub response_cache_ttl_secs: u64,
    #[serde(default)]
    pub router_ema_enabled: bool,
    #[serde(default = "default_router_ema_alpha")]
    pub router_ema_alpha: f64,
    #[serde(default = "default_router_reorder_interval")]
    pub router_reorder_interval: u64,
    /// Provider-specific instruction file to inject into the system prompt.
    /// Merged with `agent.instruction_files` at startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_file: Option<std::path::PathBuf>,
}

fn default_response_cache_ttl_secs() -> u64 {
    3600
}

fn default_router_ema_alpha() -> f64 {
    0.1
}

fn default_router_reorder_interval() -> u64 {
    10
}

fn default_embedding_model() -> String {
    "qwen3-embedding".into()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SttConfig {
    #[serde(default = "default_stt_provider")]
    pub provider: String,
    #[serde(default = "default_stt_model")]
    pub model: String,
    #[serde(default = "default_stt_language")]
    pub language: String,
    #[serde(default)]
    pub base_url: Option<String>,
}

pub(crate) fn default_stt_provider() -> String {
    "whisper".into()
}

pub(crate) fn default_stt_model() -> String {
    "whisper-1".into()
}

pub(crate) fn default_stt_language() -> String {
    "auto".into()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CloudLlmConfig {
    pub model: String,
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OllamaConfig {
    /// Enable native `tool_use` / function calling for compatible models (e.g. llama3.1, qwen2.5).
    #[serde(default)]
    pub tool_use: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
    #[serde(default)]
    pub embedding_model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CompatibleConfig {
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
    #[serde(default)]
    pub embedding_model: Option<String>,
}

/// Routing strategy selection for `[llm.router]` config.
///
/// EMA and Thompson config fields are split across `RouterConfig` and `LlmConfig`
/// for historical reasons. `RouterConfig.strategy` is the single dispatch point;
/// the `LlmConfig.router_ema_*` fields only take effect when `strategy = "ema"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RouterStrategyConfig {
    /// Exponential moving average latency-aware ordering.
    #[default]
    Ema,
    /// Thompson Sampling with Beta distributions (persistence-backed).
    Thompson,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RouterConfig {
    pub chain: Vec<String>,
    /// Routing strategy: `"ema"` (default) or `"thompson"`.
    #[serde(default)]
    pub strategy: RouterStrategyConfig,
    /// Path for persisting Thompson Sampling state. Defaults to `~/.zeph/router_thompson_state.json`.
    ///
    /// # Security
    ///
    /// This path is user-controlled. The application writes and reads a JSON file at
    /// this location. Ensure the path is within a directory that is not world-writable
    /// (e.g., avoid `/tmp`). The file is created with mode `0o600` on Unix.
    #[serde(default)]
    pub thompson_state_path: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CandleConfig {
    #[serde(default = "default_candle_source")]
    pub source: String,
    #[serde(default)]
    pub local_path: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default = "default_chat_template")]
    pub chat_template: String,
    #[serde(default = "default_candle_device")]
    pub device: String,
    #[serde(default)]
    pub embedding_repo: Option<String>,
    #[serde(default)]
    pub generation: GenerationParams,
}

fn default_candle_source() -> String {
    "huggingface".into()
}

fn default_chat_template() -> String {
    "chatml".into()
}

fn default_candle_device() -> String {
    "cpu".into()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GenerationParams {
    #[serde(default = "default_temperature")]
    pub temperature: f64,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub top_k: Option<usize>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    #[serde(default = "default_seed")]
    pub seed: u64,
    #[serde(default = "default_repeat_penalty")]
    pub repeat_penalty: f32,
    #[serde(default = "default_repeat_last_n")]
    pub repeat_last_n: usize,
}

pub(crate) const MAX_TOKENS_CAP: usize = 32768;

impl GenerationParams {
    #[must_use]
    pub fn capped_max_tokens(&self) -> usize {
        self.max_tokens.min(MAX_TOKENS_CAP)
    }
}

impl Default for GenerationParams {
    fn default() -> Self {
        Self {
            temperature: default_temperature(),
            top_p: None,
            top_k: None,
            max_tokens: default_max_tokens(),
            seed: default_seed(),
            repeat_penalty: default_repeat_penalty(),
            repeat_last_n: default_repeat_last_n(),
        }
    }
}

fn default_temperature() -> f64 {
    0.7
}

fn default_max_tokens() -> usize {
    2048
}

fn default_seed() -> u64 {
    42
}

fn default_repeat_penalty() -> f32 {
    1.1
}

fn default_repeat_last_n() -> usize {
    64
}

#[derive(Debug, Deserialize, Serialize)]
pub struct OrchestratorConfig {
    pub default: String,
    pub embed: String,
    #[serde(default)]
    pub providers: std::collections::HashMap<String, OrchestratorProviderConfig>,
    #[serde(default)]
    pub routes: std::collections::HashMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct OrchestratorProviderConfig {
    #[serde(rename = "type")]
    pub provider_type: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub embedding_model: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub device: Option<String>,
    /// Provider-specific instruction file to inject into the system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_file: Option<std::path::PathBuf>,
}

/// Controls how skills are formatted in the system prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillPromptMode {
    Full,
    Compact,
    #[default]
    Auto,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SkillsConfig {
    pub paths: Vec<String>,
    #[serde(default = "default_max_active_skills")]
    pub max_active_skills: usize,
    #[serde(default = "default_disambiguation_threshold")]
    pub disambiguation_threshold: f32,
    #[serde(default = "default_cosine_weight")]
    pub cosine_weight: f32,
    #[serde(default = "default_hybrid_search")]
    pub hybrid_search: bool,
    #[serde(default)]
    pub learning: LearningConfig,
    #[serde(default)]
    pub trust: TrustConfig,
    #[serde(default)]
    pub prompt_mode: SkillPromptMode,
}

fn default_disambiguation_threshold() -> f32 {
    0.05
}
fn default_cosine_weight() -> f32 {
    0.7
}

fn default_hybrid_search() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TrustConfig {
    #[serde(default = "default_trust_default_level")]
    pub default_level: TrustLevel,
    #[serde(default = "default_trust_local_level")]
    pub local_level: TrustLevel,
    #[serde(default = "default_trust_hash_mismatch_level")]
    pub hash_mismatch_level: TrustLevel,
}

fn default_trust_default_level() -> TrustLevel {
    TrustLevel::Quarantined
}

fn default_trust_local_level() -> TrustLevel {
    TrustLevel::Trusted
}

fn default_trust_hash_mismatch_level() -> TrustLevel {
    TrustLevel::Quarantined
}

impl Default for TrustConfig {
    fn default() -> Self {
        Self {
            default_level: default_trust_default_level(),
            local_level: default_trust_local_level(),
            hash_mismatch_level: default_trust_hash_mismatch_level(),
        }
    }
}

fn default_max_active_skills() -> usize {
    5
}

/// Strategy for detecting implicit user corrections.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DetectorMode {
    /// Pattern-matching only — zero LLM calls. Default behavior.
    #[default]
    Regex,
    /// LLM-based judge for borderline / missed cases. Invoked only when
    /// regex confidence falls below `judge_adaptive_high` or regex returns None.
    ///
    /// Note: with current regex values (0.85/0.70/0.75) and `adaptive_high=0.80`,
    /// this is effectively two-tier: `ExplicitRejection` (0.85) bypasses the judge,
    /// while `AlternativeRequest` (0.70), `Repetition` (0.75), and regex misses go through it.
    Judge,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LearningConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub auto_activate: bool,
    #[serde(default = "default_min_failures")]
    pub min_failures: u32,
    #[serde(default = "default_improve_threshold")]
    pub improve_threshold: f64,
    #[serde(default = "default_rollback_threshold")]
    pub rollback_threshold: f64,
    #[serde(default = "default_min_evaluations")]
    pub min_evaluations: u32,
    #[serde(default = "default_max_versions")]
    pub max_versions: u32,
    #[serde(default = "default_cooldown_minutes")]
    pub cooldown_minutes: u64,
    #[serde(default = "default_correction_detection")]
    pub correction_detection: bool,
    #[serde(default = "default_correction_confidence_threshold")]
    pub correction_confidence_threshold: f32,
    /// Detector strategy: "regex" (default) or "judge".
    #[serde(default)]
    pub detector_mode: DetectorMode,
    /// Model for the judge detector (e.g. "claude-sonnet-4-6"). Empty = use primary provider.
    #[serde(default)]
    pub judge_model: String,
    /// Regex confidence below this value is treated as "not a correction" — judge not invoked.
    #[serde(default = "default_judge_adaptive_low")]
    pub judge_adaptive_low: f32,
    /// Regex confidence at or above this value is accepted without judge confirmation.
    #[serde(default = "default_judge_adaptive_high")]
    pub judge_adaptive_high: f32,
    #[serde(default = "default_correction_recall_limit")]
    pub correction_recall_limit: u32,
    #[serde(default = "default_correction_min_similarity")]
    pub correction_min_similarity: f32,
    #[serde(default = "default_auto_promote_min_uses")]
    pub auto_promote_min_uses: u32,
    #[serde(default = "default_auto_promote_threshold")]
    pub auto_promote_threshold: f64,
    #[serde(default = "default_auto_demote_min_uses")]
    pub auto_demote_min_uses: u32,
    #[serde(default = "default_auto_demote_threshold")]
    pub auto_demote_threshold: f64,
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_activate: false,
            min_failures: default_min_failures(),
            improve_threshold: default_improve_threshold(),
            rollback_threshold: default_rollback_threshold(),
            min_evaluations: default_min_evaluations(),
            max_versions: default_max_versions(),
            cooldown_minutes: default_cooldown_minutes(),
            correction_detection: default_correction_detection(),
            correction_confidence_threshold: default_correction_confidence_threshold(),
            detector_mode: DetectorMode::default(),
            judge_model: String::new(),
            judge_adaptive_low: default_judge_adaptive_low(),
            judge_adaptive_high: default_judge_adaptive_high(),
            correction_recall_limit: default_correction_recall_limit(),
            correction_min_similarity: default_correction_min_similarity(),
            auto_promote_min_uses: default_auto_promote_min_uses(),
            auto_promote_threshold: default_auto_promote_threshold(),
            auto_demote_min_uses: default_auto_demote_min_uses(),
            auto_demote_threshold: default_auto_demote_threshold(),
        }
    }
}

fn default_min_failures() -> u32 {
    3
}
fn default_improve_threshold() -> f64 {
    0.7
}
fn default_rollback_threshold() -> f64 {
    0.5
}
fn default_min_evaluations() -> u32 {
    5
}
fn default_max_versions() -> u32 {
    10
}
fn default_cooldown_minutes() -> u64 {
    60
}
fn default_correction_detection() -> bool {
    true
}
fn default_correction_confidence_threshold() -> f32 {
    0.6
}
fn default_judge_adaptive_low() -> f32 {
    0.5
}
fn default_judge_adaptive_high() -> f32 {
    0.8
}
fn default_correction_recall_limit() -> u32 {
    3
}
fn default_correction_min_similarity() -> f32 {
    0.75
}
fn default_auto_promote_min_uses() -> u32 {
    50
}
fn default_auto_promote_threshold() -> f64 {
    0.95
}
fn default_auto_demote_min_uses() -> u32 {
    30
}
fn default_auto_demote_threshold() -> f64 {
    0.40
}

/// Vector backend selector for embedding storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VectorBackend {
    #[default]
    Qdrant,
    Sqlite,
}

impl VectorBackend {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Qdrant => "qdrant",
            Self::Sqlite => "sqlite",
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MemoryConfig {
    pub sqlite_path: String,
    pub history_limit: u32,
    #[serde(default = "default_qdrant_url")]
    pub qdrant_url: String,
    #[serde(default)]
    pub semantic: SemanticConfig,
    #[serde(default = "default_summarization_threshold")]
    pub summarization_threshold: usize,
    #[serde(default = "default_context_budget_tokens")]
    pub context_budget_tokens: usize,
    #[serde(default = "default_compaction_threshold")]
    pub compaction_threshold: f32,
    #[serde(default = "default_compaction_preserve_tail")]
    pub compaction_preserve_tail: usize,
    #[serde(default = "default_auto_budget")]
    pub auto_budget: bool,
    #[serde(default = "default_prune_protect_tokens")]
    pub prune_protect_tokens: usize,
    #[serde(default = "default_cross_session_score_threshold")]
    pub cross_session_score_threshold: f32,
    #[serde(default)]
    pub vector_backend: VectorBackend,
    #[serde(default = "default_token_safety_margin")]
    pub token_safety_margin: f32,
    #[serde(default = "default_redact_credentials")]
    pub redact_credentials: bool,
    #[serde(default)]
    pub autosave_assistant: bool,
    #[serde(default = "default_autosave_min_length")]
    pub autosave_min_length: usize,
    #[serde(default = "default_tool_call_cutoff")]
    pub tool_call_cutoff: usize,
    #[serde(default = "default_sqlite_pool_size")]
    pub sqlite_pool_size: u32,
    #[serde(default)]
    pub sessions: SessionsConfig,
    #[serde(default)]
    pub documents: DocumentConfig,
    #[serde(default)]
    pub eviction: zeph_memory::EvictionConfig,
    #[serde(default)]
    pub compression: CompressionConfig,
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub graph: GraphConfig,
}

fn default_sqlite_pool_size() -> u32 {
    5
}

fn default_max_history() -> usize {
    100
}

fn default_title_max_chars() -> usize {
    60
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SessionsConfig {
    /// Maximum number of sessions returned by list operations (0 = unlimited).
    #[serde(default = "default_max_history")]
    pub max_history: usize,
    /// Maximum characters for auto-generated session titles.
    #[serde(default = "default_title_max_chars")]
    pub title_max_chars: usize,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            max_history: default_max_history(),
            title_max_chars: default_title_max_chars(),
        }
    }
}

fn default_document_collection() -> String {
    "zeph_documents".into()
}

fn default_document_chunk_size() -> usize {
    1000
}

fn default_document_chunk_overlap() -> usize {
    100
}

fn default_document_top_k() -> usize {
    3
}

/// Configuration for the document ingestion and RAG retrieval pipeline.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DocumentConfig {
    #[serde(default = "default_document_collection")]
    pub collection: String,
    #[serde(default = "default_document_chunk_size")]
    pub chunk_size: usize,
    #[serde(default = "default_document_chunk_overlap")]
    pub chunk_overlap: usize,
    /// Number of document chunks to inject into agent context per turn.
    #[serde(default = "default_document_top_k")]
    pub top_k: usize,
    /// Enable document RAG injection into agent context.
    #[serde(default)]
    pub rag_enabled: bool,
}

impl Default for DocumentConfig {
    fn default() -> Self {
        Self {
            collection: default_document_collection(),
            chunk_size: default_document_chunk_size(),
            chunk_overlap: default_document_chunk_overlap(),
            top_k: default_document_top_k(),
            rag_enabled: false,
        }
    }
}

fn default_autosave_min_length() -> usize {
    20
}

fn default_tool_call_cutoff() -> usize {
    6
}

fn default_token_safety_margin() -> f32 {
    1.0
}

fn default_redact_credentials() -> bool {
    true
}

fn default_qdrant_url() -> String {
    "http://localhost:6334".into()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct IndexConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_index_watch")]
    pub watch: bool,
    #[serde(default = "default_index_max_chunks")]
    pub max_chunks: usize,
    #[serde(default = "default_index_score_threshold")]
    pub score_threshold: f32,
    #[serde(default = "default_index_budget_ratio")]
    pub budget_ratio: f32,
    #[serde(default = "default_index_repo_map_tokens")]
    pub repo_map_tokens: usize,
    #[serde(default = "default_repo_map_ttl_secs")]
    pub repo_map_ttl_secs: u64,
}

fn default_index_watch() -> bool {
    true
}

fn default_index_max_chunks() -> usize {
    12
}

fn default_index_score_threshold() -> f32 {
    0.25
}

fn default_index_budget_ratio() -> f32 {
    0.40
}

fn default_index_repo_map_tokens() -> usize {
    500
}

fn default_repo_map_ttl_secs() -> u64 {
    300
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            watch: default_index_watch(),
            max_chunks: default_index_max_chunks(),
            score_threshold: default_index_score_threshold(),
            budget_ratio: default_index_budget_ratio(),
            repo_map_tokens: default_index_repo_map_tokens(),
            repo_map_ttl_secs: default_repo_map_ttl_secs(),
        }
    }
}

fn default_summarization_threshold() -> usize {
    50
}

fn default_context_budget_tokens() -> usize {
    0
}

fn default_compaction_threshold() -> f32 {
    0.80
}

fn default_compaction_preserve_tail() -> usize {
    6
}

fn default_auto_budget() -> bool {
    true
}

fn default_prune_protect_tokens() -> usize {
    40_000
}

fn default_cross_session_score_threshold() -> f32 {
    0.35
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SemanticConfig {
    #[serde(default = "default_semantic_enabled")]
    pub enabled: bool,
    #[serde(default = "default_recall_limit")]
    pub recall_limit: usize,
    #[serde(default = "default_vector_weight")]
    pub vector_weight: f64,
    #[serde(default = "default_keyword_weight")]
    pub keyword_weight: f64,
    #[serde(default)]
    pub temporal_decay_enabled: bool,
    #[serde(default = "default_temporal_decay_half_life_days")]
    pub temporal_decay_half_life_days: u32,
    #[serde(default)]
    pub mmr_enabled: bool,
    #[serde(default = "default_mmr_lambda")]
    pub mmr_lambda: f32,
}

fn default_temporal_decay_half_life_days() -> u32 {
    30
}

fn default_mmr_lambda() -> f32 {
    0.7
}

impl Default for SemanticConfig {
    fn default() -> Self {
        Self {
            enabled: default_semantic_enabled(),
            recall_limit: default_recall_limit(),
            vector_weight: default_vector_weight(),
            keyword_weight: default_keyword_weight(),
            temporal_decay_enabled: false,
            temporal_decay_half_life_days: default_temporal_decay_half_life_days(),
            mmr_enabled: false,
            mmr_lambda: default_mmr_lambda(),
        }
    }
}

fn default_semantic_enabled() -> bool {
    true
}

fn default_recall_limit() -> usize {
    5
}

fn default_vector_weight() -> f64 {
    0.7
}

fn default_keyword_weight() -> f64 {
    0.3
}

#[derive(Clone, Deserialize, Serialize)]
pub struct TelegramConfig {
    pub token: Option<String>,
    #[serde(default)]
    pub allowed_users: Vec<String>,
}

impl std::fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("allowed_users", &self.allowed_users)
            .finish()
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct DiscordConfig {
    pub token: Option<String>,
    pub application_id: Option<String>,
    #[serde(default)]
    pub allowed_user_ids: Vec<String>,
    #[serde(default)]
    pub allowed_role_ids: Vec<String>,
    #[serde(default)]
    pub allowed_channel_ids: Vec<String>,
}

impl std::fmt::Debug for DiscordConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordConfig")
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("application_id", &self.application_id)
            .field("allowed_user_ids", &self.allowed_user_ids)
            .field("allowed_role_ids", &self.allowed_role_ids)
            .field("allowed_channel_ids", &self.allowed_channel_ids)
            .finish()
    }
}

fn default_slack_port() -> u16 {
    3000
}

fn default_slack_webhook_host() -> String {
    "127.0.0.1".into()
}

#[derive(Clone, Deserialize, Serialize)]
pub struct SlackConfig {
    pub bot_token: Option<String>,
    pub signing_secret: Option<String>,
    #[serde(default = "default_slack_webhook_host")]
    pub webhook_host: String,
    #[serde(default = "default_slack_port")]
    pub port: u16,
    #[serde(default)]
    pub allowed_user_ids: Vec<String>,
    #[serde(default)]
    pub allowed_channel_ids: Vec<String>,
}

impl std::fmt::Debug for SlackConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackConfig")
            .field("bot_token", &self.bot_token.as_ref().map(|_| "[REDACTED]"))
            .field(
                "signing_secret",
                &self.signing_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field("webhook_host", &self.webhook_host)
            .field("port", &self.port)
            .field("allowed_user_ids", &self.allowed_user_ids)
            .field("allowed_channel_ids", &self.allowed_channel_ids)
            .finish()
    }
}

#[derive(Deserialize, Serialize)]
pub struct A2aServerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_a2a_host")]
    pub host: String,
    #[serde(default = "default_a2a_port")]
    pub port: u16,
    #[serde(default)]
    pub public_url: String,
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default = "default_a2a_rate_limit")]
    pub rate_limit: u32,
    #[serde(default = "default_true")]
    pub require_tls: bool,
    #[serde(default = "default_true")]
    pub ssrf_protection: bool,
    #[serde(default = "default_a2a_max_body")]
    pub max_body_size: usize,
}

fn default_true() -> bool {
    true
}

fn default_a2a_max_body() -> usize {
    1_048_576
}

impl std::fmt::Debug for A2aServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aServerConfig")
            .field("enabled", &self.enabled)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("public_url", &self.public_url)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("rate_limit", &self.rate_limit)
            .field("require_tls", &self.require_tls)
            .field("ssrf_protection", &self.ssrf_protection)
            .field("max_body_size", &self.max_body_size)
            .finish()
    }
}

fn default_a2a_host() -> String {
    "0.0.0.0".into()
}

fn default_a2a_port() -> u16 {
    8080
}

fn default_a2a_rate_limit() -> u32 {
    60
}

impl Default for A2aServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: default_a2a_host(),
            port: default_a2a_port(),
            public_url: String::new(),
            auth_token: None,
            rate_limit: default_a2a_rate_limit(),
            require_tls: true,
            ssrf_protection: true,
            max_body_size: default_a2a_max_body(),
        }
    }
}

fn default_llm_timeout() -> u64 {
    120
}

fn default_embedding_timeout() -> u64 {
    30
}

fn default_a2a_timeout() -> u64 {
    30
}

fn default_max_parallel_tools() -> usize {
    8
}

fn default_llm_request_timeout() -> u64 {
    600
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SecurityConfig {
    #[serde(default = "default_true")]
    pub redact_secrets: bool,
    #[serde(default)]
    pub autonomy_level: AutonomyLevel,
    #[serde(default)]
    pub content_isolation: ContentIsolationConfig,
    #[serde(default)]
    pub exfiltration_guard: ExfiltrationGuardConfig,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            redact_secrets: true,
            autonomy_level: AutonomyLevel::default(),
            content_isolation: ContentIsolationConfig::default(),
            exfiltration_guard: ExfiltrationGuardConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct TimeoutConfig {
    #[serde(default = "default_llm_timeout")]
    pub llm_seconds: u64,
    #[serde(default = "default_llm_request_timeout")]
    pub llm_request_timeout_secs: u64,
    #[serde(default = "default_embedding_timeout")]
    pub embedding_seconds: u64,
    #[serde(default = "default_a2a_timeout")]
    pub a2a_seconds: u64,
    #[serde(default = "default_max_parallel_tools")]
    pub max_parallel_tools: usize,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            llm_seconds: default_llm_timeout(),
            llm_request_timeout_secs: default_llm_request_timeout(),
            embedding_seconds: default_embedding_timeout(),
            a2a_seconds: default_a2a_timeout(),
            max_parallel_tools: default_max_parallel_tools(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
    #[serde(default)]
    pub allowed_commands: Vec<String>,
    #[serde(default = "default_max_dynamic_servers")]
    pub max_dynamic_servers: usize,
}

fn default_max_dynamic_servers() -> usize {
    10
}

#[derive(Clone, Deserialize, Serialize)]
pub struct McpServerConfig {
    pub id: String,
    /// Stdio transport: command to spawn.
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// HTTP transport: remote MCP server URL.
    pub url: Option<String>,
    #[serde(default = "default_mcp_timeout")]
    pub timeout: u64,
    /// Optional declarative policy for this server (allowlist, denylist, rate limit).
    #[serde(default)]
    pub policy: zeph_mcp::McpPolicy,
}

impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redacted: HashMap<&str, &str> = self
            .env
            .keys()
            .map(|k| (k.as_str(), "[REDACTED]"))
            .collect();
        f.debug_struct("McpServerConfig")
            .field("id", &self.id)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env", &redacted)
            .field("url", &self.url)
            .field("timeout", &self.timeout)
            .field("policy", &self.policy)
            .finish()
    }
}

fn default_mcp_timeout() -> u64 {
    30
}

#[derive(Debug, Deserialize, Serialize)]
pub struct VaultConfig {
    #[serde(default = "default_vault_backend")]
    pub backend: String,
}

impl Default for VaultConfig {
    fn default() -> Self {
        Self {
            backend: default_vault_backend(),
        }
    }
}

fn default_vault_backend() -> String {
    "env".into()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CostConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_max_daily_cents")]
    pub max_daily_cents: u32,
}

fn default_max_daily_cents() -> u32 {
    500
}

impl Default for CostConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_daily_cents: default_max_daily_cents(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ObservabilityConfig {
    #[serde(default)]
    pub exporter: String,
    #[serde(default = "default_otlp_endpoint")]
    pub endpoint: String,
}

fn default_otlp_endpoint() -> String {
    "http://localhost:4317".into()
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            exporter: String::new(),
            endpoint: default_otlp_endpoint(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewayConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_gateway_bind")]
    pub bind: String,
    #[serde(default = "default_gateway_port")]
    pub port: u16,
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default = "default_gateway_rate_limit")]
    pub rate_limit: u32,
    #[serde(default = "default_gateway_max_body")]
    pub max_body_size: usize,
}

fn default_gateway_bind() -> String {
    "127.0.0.1".into()
}

fn default_gateway_port() -> u16 {
    8090
}

fn default_gateway_rate_limit() -> u32 {
    120
}

fn default_gateway_max_body() -> usize {
    1_048_576
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_gateway_bind(),
            port: default_gateway_port(),
            auth_token: None,
            rate_limit: default_gateway_rate_limit(),
            max_body_size: default_gateway_max_body(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DaemonConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_pid_file")]
    pub pid_file: String,
    #[serde(default = "default_health_interval")]
    pub health_interval_secs: u64,
    #[serde(default = "default_max_restart_backoff")]
    pub max_restart_backoff_secs: u64,
}

fn default_pid_file() -> String {
    "~/.zeph/zeph.pid".into()
}

fn default_health_interval() -> u64 {
    30
}

fn default_max_restart_backoff() -> u64 {
    60
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            pid_file: default_pid_file(),
            health_interval_secs: default_health_interval(),
            max_restart_backoff_secs: default_max_restart_backoff(),
        }
    }
}

fn default_scheduler_tick_interval() -> u64 {
    60
}

fn default_scheduler_max_tasks() -> usize {
    100
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchedulerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_scheduler_tick_interval")]
    pub tick_interval_secs: u64,
    #[serde(default = "default_scheduler_max_tasks")]
    pub max_tasks: usize,
    #[serde(default)]
    pub tasks: Vec<ScheduledTaskConfig>,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tick_interval_secs: default_scheduler_tick_interval(),
            max_tasks: default_scheduler_max_tasks(),
            tasks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
pub struct TuiConfig {
    #[serde(default)]
    pub show_source_labels: bool,
}

fn default_acp_agent_name() -> String {
    "zeph".to_owned()
}

fn default_acp_agent_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

fn default_acp_max_sessions() -> usize {
    4
}

fn default_acp_session_idle_timeout_secs() -> u64 {
    1800
}

fn default_acp_transport() -> AcpTransport {
    AcpTransport::Stdio
}

fn default_acp_http_bind() -> String {
    "127.0.0.1:9800".to_owned()
}

/// ACP server transport mode.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AcpTransport {
    /// JSON-RPC over stdin/stdout (default, IDE embedding).
    #[default]
    Stdio,
    /// JSON-RPC over HTTP+SSE and WebSocket.
    Http,
    /// Both stdio and HTTP transports active simultaneously.
    Both,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct AcpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_acp_agent_name")]
    pub agent_name: String,
    #[serde(default = "default_acp_agent_version")]
    pub agent_version: String,
    #[serde(default = "default_acp_max_sessions")]
    pub max_sessions: usize,
    #[serde(default = "default_acp_session_idle_timeout_secs")]
    pub session_idle_timeout_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_file: Option<std::path::PathBuf>,
    /// List of `{provider}:{model}` identifiers advertised to the IDE for model switching.
    /// Example: `["claude:claude-sonnet-4-5", "ollama:llama3"]`
    #[serde(default)]
    pub available_models: Vec<String>,
    /// Transport mode: "stdio" (default), "http", or "both".
    #[serde(default = "default_acp_transport")]
    pub transport: AcpTransport,
    /// Bind address for the HTTP transport.
    #[serde(default = "default_acp_http_bind")]
    pub http_bind: String,
    /// Bearer token for HTTP and WebSocket transport authentication.
    /// When set, all /acp and /acp/ws requests must include `Authorization: Bearer <token>`.
    /// Omit for local unauthenticated access. TLS termination is assumed to be handled by a
    /// reverse proxy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    /// Whether to serve the /.well-known/acp.json agent discovery manifest.
    /// Only effective when transport is "http" or "both". Default: true.
    #[serde(default = "default_acp_discovery_enabled")]
    pub discovery_enabled: bool,
}

fn default_acp_discovery_enabled() -> bool {
    true
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            agent_name: default_acp_agent_name(),
            agent_version: default_acp_agent_version(),
            max_sessions: default_acp_max_sessions(),
            session_idle_timeout_secs: default_acp_session_idle_timeout_secs(),
            permission_file: None,
            available_models: Vec::new(),
            transport: default_acp_transport(),
            http_bind: default_acp_http_bind(),
            auth_token: None,
            discovery_enabled: default_acp_discovery_enabled(),
        }
    }
}

impl std::fmt::Debug for AcpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpConfig")
            .field("enabled", &self.enabled)
            .field("agent_name", &self.agent_name)
            .field("agent_version", &self.agent_version)
            .field("max_sessions", &self.max_sessions)
            .field("session_idle_timeout_secs", &self.session_idle_timeout_secs)
            .field("permission_file", &self.permission_file)
            .field("available_models", &self.available_models)
            .field("transport", &self.transport)
            .field("http_bind", &self.http_bind)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("discovery_enabled", &self.discovery_enabled)
            .finish()
    }
}

/// Task kind for scheduled tasks.
///
/// Known variants map to built-in handlers; `Custom` accommodates user-defined task types.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledTaskKind {
    MemoryCleanup,
    SkillRefresh,
    HealthCheck,
    UpdateCheck,
    Custom(String),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScheduledTaskConfig {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_at: Option<String>,
    pub kind: ScheduledTaskKind,
    #[serde(default)]
    pub config: serde_json::Value,
}

#[derive(Debug, Default)]
pub struct ResolvedSecrets {
    pub claude_api_key: Option<Secret>,
    pub openai_api_key: Option<Secret>,
    pub compatible_api_keys: HashMap<String, Secret>,
    pub discord_token: Option<Secret>,
    pub slack_bot_token: Option<Secret>,
    pub slack_signing_secret: Option<Secret>,
    /// Arbitrary skill secrets resolved from `ZEPH_SECRET_*` vault keys.
    /// Key is the lowercased name after stripping the prefix (e.g. `github_token`).
    pub custom: HashMap<String, Secret>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            agent: AgentConfig {
                name: "Zeph".into(),
                max_tool_iterations: 10,
                summary_model: None,
                auto_update_check: default_auto_update_check(),
                instruction_files: Vec::new(),
                instruction_auto_detect: default_instruction_auto_detect(),
            },
            llm: LlmConfig {
                provider: ProviderKind::Ollama,
                base_url: "http://localhost:11434".into(),
                model: "qwen3:8b".into(),
                embedding_model: default_embedding_model(),
                cloud: None,
                ollama: None,
                openai: None,
                candle: None,
                orchestrator: None,
                compatible: None,
                router: None,
                stt: None,
                vision_model: None,
                response_cache_enabled: false,
                response_cache_ttl_secs: default_response_cache_ttl_secs(),
                router_ema_enabled: false,
                router_ema_alpha: default_router_ema_alpha(),
                router_reorder_interval: default_router_reorder_interval(),
                instruction_file: None,
            },
            skills: SkillsConfig {
                paths: vec!["./skills".into()],
                max_active_skills: default_max_active_skills(),
                disambiguation_threshold: default_disambiguation_threshold(),
                cosine_weight: default_cosine_weight(),
                hybrid_search: default_hybrid_search(),
                learning: LearningConfig::default(),
                trust: TrustConfig::default(),
                prompt_mode: SkillPromptMode::Auto,
            },
            memory: MemoryConfig {
                sqlite_path: "./data/zeph.db".into(),
                history_limit: 50,
                qdrant_url: default_qdrant_url(),
                semantic: SemanticConfig::default(),
                summarization_threshold: default_summarization_threshold(),
                context_budget_tokens: default_context_budget_tokens(),
                compaction_threshold: default_compaction_threshold(),
                compaction_preserve_tail: default_compaction_preserve_tail(),
                auto_budget: default_auto_budget(),
                prune_protect_tokens: default_prune_protect_tokens(),
                cross_session_score_threshold: default_cross_session_score_threshold(),
                vector_backend: VectorBackend::default(),
                token_safety_margin: default_token_safety_margin(),
                redact_credentials: default_redact_credentials(),
                autosave_assistant: false,
                autosave_min_length: default_autosave_min_length(),
                tool_call_cutoff: default_tool_call_cutoff(),
                sqlite_pool_size: default_sqlite_pool_size(),
                sessions: SessionsConfig::default(),
                documents: DocumentConfig::default(),
                eviction: zeph_memory::EvictionConfig::default(),
                compression: CompressionConfig::default(),
                routing: RoutingConfig::default(),
                graph: GraphConfig::default(),
            },
            telegram: None,
            discord: None,
            slack: None,
            tools: ToolsConfig::default(),
            a2a: A2aServerConfig::default(),
            mcp: McpConfig::default(),
            index: IndexConfig::default(),
            vault: VaultConfig::default(),
            security: SecurityConfig::default(),
            timeouts: TimeoutConfig::default(),
            cost: CostConfig::default(),
            observability: ObservabilityConfig::default(),
            gateway: GatewayConfig::default(),
            daemon: DaemonConfig::default(),
            scheduler: SchedulerConfig::default(),
            tui: TuiConfig::default(),
            acp: AcpConfig::default(),
            agents: SubAgentConfig::default(),
            orchestration: OrchestrationConfig::default(),
            secrets: ResolvedSecrets::default(),
        }
    }
}

/// Routing strategy for memory backend selection.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoutingStrategy {
    /// Heuristic-based routing using query characteristics.
    #[default]
    Heuristic,
}

/// Configuration for query-aware memory routing (#1162).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RoutingConfig {
    /// Routing strategy. Currently only `heuristic` is supported.
    pub strategy: RoutingStrategy,
}

fn default_graph_max_entities_per_message() -> usize {
    10
}

fn default_graph_max_edges_per_message() -> usize {
    15
}

fn default_graph_community_refresh_interval() -> usize {
    100
}

fn default_graph_entity_similarity_threshold() -> f32 {
    0.85
}

fn default_graph_extraction_timeout_secs() -> u64 {
    15
}

fn default_graph_max_hops() -> u32 {
    2
}

fn default_graph_recall_limit() -> usize {
    10
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct GraphConfig {
    pub enabled: bool,
    pub extract_model: String,
    #[serde(default = "default_graph_max_entities_per_message")]
    pub max_entities_per_message: usize,
    #[serde(default = "default_graph_max_edges_per_message")]
    pub max_edges_per_message: usize,
    #[serde(default = "default_graph_community_refresh_interval")]
    pub community_refresh_interval: usize,
    #[serde(default = "default_graph_entity_similarity_threshold")]
    pub entity_similarity_threshold: f32,
    #[serde(default = "default_graph_extraction_timeout_secs")]
    pub extraction_timeout_secs: u64,
    #[serde(default)]
    pub use_embedding_resolution: bool,
    #[serde(default = "default_graph_max_hops")]
    pub max_hops: u32,
    #[serde(default = "default_graph_recall_limit")]
    pub recall_limit: usize,
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            extract_model: String::new(),
            max_entities_per_message: default_graph_max_entities_per_message(),
            max_edges_per_message: default_graph_max_edges_per_message(),
            community_refresh_interval: default_graph_community_refresh_interval(),
            entity_similarity_threshold: default_graph_entity_similarity_threshold(),
            extraction_timeout_secs: default_graph_extraction_timeout_secs(),
            use_embedding_resolution: false,
            max_hops: default_graph_max_hops(),
            recall_limit: default_graph_recall_limit(),
        }
    }
}

/// Configuration for the task orchestration subsystem (`[orchestration]` TOML section).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct OrchestrationConfig {
    /// Enable the orchestration subsystem.
    pub enabled: bool,
    /// Maximum number of tasks in a single graph.
    pub max_tasks: u32,
    /// Maximum number of tasks that can run in parallel.
    pub max_parallel: u32,
    /// Default failure strategy for all tasks unless overridden per-task.
    pub default_failure_strategy: String,
    /// Default number of retries for the `retry` failure strategy.
    pub default_max_retries: u32,
    /// Timeout in seconds for a single task. `0` means no timeout.
    pub task_timeout_secs: u64,
}

impl Default for OrchestrationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_tasks: 20,
            max_parallel: 4,
            default_failure_strategy: "abort".to_string(),
            default_max_retries: 3,
            task_timeout_secs: 300,
        }
    }
}

#[cfg(feature = "orchestration")]
impl OrchestrationConfig {
    /// Parse and validate `default_failure_strategy` as a typed `FailureStrategy`.
    ///
    /// Called at orchestration startup to validate the config value.
    ///
    /// # Errors
    ///
    /// Returns `OrchestrationError::InvalidGraph` if the string is not one of
    /// `abort`, `retry`, `skip`, `ask`.
    pub fn failure_strategy(
        &self,
    ) -> Result<crate::orchestration::FailureStrategy, crate::orchestration::OrchestrationError>
    {
        self.default_failure_strategy.parse()
    }
}

/// Compression strategy for active context compression (#1161).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(tag = "strategy", rename_all = "snake_case")]
pub enum CompressionStrategy {
    /// Compress only when reactive compaction fires (current behavior).
    #[default]
    Reactive,
    /// Compress proactively when context exceeds `threshold_tokens`.
    Proactive {
        /// Token count that triggers proactive compression.
        threshold_tokens: usize,
        /// Maximum tokens for the compressed summary (passed to LLM as `max_tokens`).
        max_summary_tokens: usize,
    },
}

/// Configuration for active context compression (#1161).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct CompressionConfig {
    /// Compression strategy.
    #[serde(flatten)]
    pub strategy: CompressionStrategy,
    /// Model to use for compression summaries.
    ///
    /// Currently unused — the primary summary provider is used regardless of this value.
    /// Reserved for future per-compression model selection. Setting this field has no effect.
    pub model: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_serialize_roundtrip() {
        let config = Config::default();
        let toml_str = toml::to_string_pretty(&config).expect("serialize");
        let back: Config = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(back.agent.name, config.agent.name);
        assert_eq!(back.llm.provider, config.llm.provider);
        assert_eq!(back.llm.model, config.llm.model);
        assert_eq!(back.memory.sqlite_path, config.memory.sqlite_path);
        assert_eq!(back.memory.history_limit, config.memory.history_limit);
        assert_eq!(back.vault.backend, config.vault.backend);
        assert_eq!(back.agent.auto_update_check, config.agent.auto_update_check);
    }

    #[test]
    fn config_default_snapshot() {
        let config = Config::default();
        let toml_str = toml::to_string_pretty(&config).expect("serialize");
        insta::assert_snapshot!(toml_str);
    }

    #[test]
    fn orchestration_config_defaults() {
        let cfg = OrchestrationConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.max_tasks, 20);
        assert_eq!(cfg.max_parallel, 4);
        assert_eq!(cfg.default_failure_strategy, "abort");
        assert_eq!(cfg.default_max_retries, 3);
    }

    #[test]
    fn orchestration_config_serde_roundtrip() {
        let toml_str = "enabled = true\nmax_tasks = 10\ndefault_failure_strategy = \"skip\"\n";
        let cfg: OrchestrationConfig = toml::from_str(toml_str).expect("parse");
        assert!(cfg.enabled);
        assert_eq!(cfg.max_tasks, 10);
        assert_eq!(cfg.default_failure_strategy, "skip");
    }

    #[cfg(feature = "orchestration")]
    #[test]
    fn orchestration_config_failure_strategy_valid() {
        let cfg = OrchestrationConfig::default(); // "abort"
        let fs = cfg.failure_strategy().expect("should parse");
        assert_eq!(fs, crate::orchestration::FailureStrategy::Abort);
    }

    #[cfg(feature = "orchestration")]
    #[test]
    fn orchestration_config_failure_strategy_invalid() {
        let cfg = OrchestrationConfig {
            default_failure_strategy: "abort_all".to_string(),
            ..Default::default()
        };
        assert!(cfg.failure_strategy().is_err());
    }

    #[test]
    fn generation_params_defaults() {
        let p = GenerationParams::default();
        assert!((p.temperature - 0.7).abs() < f64::EPSILON);
        assert_eq!(p.max_tokens, 2048);
        assert_eq!(p.seed, 42);
    }

    #[test]
    fn scheduled_task_kind_serde_memory_cleanup() {
        let kind = ScheduledTaskKind::MemoryCleanup;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, r#""memory_cleanup""#);
        let back: ScheduledTaskKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, kind);
    }

    #[test]
    fn scheduled_task_kind_serde_skill_refresh() {
        let kind = ScheduledTaskKind::SkillRefresh;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, r#""skill_refresh""#);
        let back: ScheduledTaskKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, kind);
    }

    #[test]
    fn scheduled_task_kind_serde_health_check() {
        let kind = ScheduledTaskKind::HealthCheck;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, r#""health_check""#);
        let back: ScheduledTaskKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, kind);
    }

    #[test]
    fn scheduled_task_kind_serde_update_check() {
        let kind = ScheduledTaskKind::UpdateCheck;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, r#""update_check""#);
        let back: ScheduledTaskKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, kind);
    }

    #[test]
    fn scheduled_task_kind_serde_custom_roundtrip() {
        let kind = ScheduledTaskKind::Custom("my_task".to_owned());
        let json = serde_json::to_string(&kind).unwrap();
        let back: ScheduledTaskKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, kind);
    }

    #[test]
    fn scheduled_task_config_toml_known_kind() {
        let toml = r#"
            name = "cleanup"
            cron = "0 3 * * *"
            kind = "memory_cleanup"
        "#;
        let cfg: ScheduledTaskConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.kind, ScheduledTaskKind::MemoryCleanup);
        assert_eq!(cfg.name, "cleanup");
    }

    #[test]
    fn scheduled_task_config_toml_custom_kind() {
        let toml = r#"
            name = "my-job"
            cron = "*/5 * * * *"
            kind = { custom = "report_gen" }
        "#;
        let cfg: ScheduledTaskConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.kind, ScheduledTaskKind::Custom("report_gen".to_owned()));
    }

    #[test]
    fn scheduled_task_config_toml_invalid_kind_errors() {
        let toml = r#"
            name = "bad"
            cron = "* * * * *"
            kind = "does_not_exist"
        "#;
        let result: Result<ScheduledTaskConfig, _> = toml::from_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn scheduled_task_config_oneshot_with_run_at() {
        let toml = r#"
            name = "reminder"
            run_at = "2026-04-01T09:00:00Z"
            kind = { custom = "my_job" }
        "#;
        let cfg: ScheduledTaskConfig = toml::from_str(toml).unwrap();
        assert!(cfg.cron.is_none());
        assert_eq!(cfg.run_at.as_deref(), Some("2026-04-01T09:00:00Z"));
        assert_eq!(cfg.kind, ScheduledTaskKind::Custom("my_job".to_owned()));
    }

    #[test]
    fn config_rejects_both_cron_and_run_at() {
        // Both set: application should validate, struct itself accepts both for flexibility.
        // The validation is done at bootstrap, not at deserialization.
        let toml = r#"
            name = "bad"
            cron = "0 * * * * *"
            run_at = "2026-04-01T09:00:00Z"
            kind = "health_check"
        "#;
        let cfg: ScheduledTaskConfig = toml::from_str(toml).unwrap();
        assert!(cfg.cron.is_some() && cfg.run_at.is_some());
    }

    #[test]
    fn scheduler_config_defaults() {
        let cfg = SchedulerConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.tick_interval_secs, 60);
        assert_eq!(cfg.max_tasks, 100);
        assert!(cfg.tasks.is_empty());
    }

    #[test]
    fn memory_config_sqlite_pool_size_default_is_5() {
        let config = Config::default();
        assert_eq!(config.memory.sqlite_pool_size, 5);
    }

    #[test]
    fn memory_config_sqlite_pool_size_deserializes_from_toml() {
        let toml = r#"
            sqlite_path = "test.db"
            history_limit = 50
            sqlite_pool_size = 10
        "#;
        let cfg: MemoryConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.sqlite_pool_size, 10);
    }

    #[test]
    fn memory_config_sqlite_pool_size_uses_default_when_absent() {
        let toml = r#"
            sqlite_path = "test.db"
            history_limit = 50
        "#;
        let cfg: MemoryConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.sqlite_pool_size, 5);
    }

    #[test]
    fn subagent_config_defaults_when_section_absent() {
        let cfg = SubAgentConfig::default();
        assert!(!cfg.enabled, "enabled defaults to false");
        assert_eq!(cfg.max_concurrent, 1, "max_concurrent defaults to 1");
        assert!(cfg.extra_dirs.is_empty(), "extra_dirs defaults to empty");

        let default_cfg = Config::default();
        assert!(!default_cfg.agents.enabled);
        assert_eq!(default_cfg.agents.max_concurrent, 1);
        assert!(default_cfg.agents.extra_dirs.is_empty());
    }

    #[test]
    fn subagent_config_full_section_deserializes() {
        let toml = r#"
            enabled = true
            max_concurrent = 8
            extra_dirs = ["/custom/agents", "/other/agents"]
        "#;
        let cfg: SubAgentConfig = toml::from_str(toml).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.max_concurrent, 8);
        assert_eq!(cfg.extra_dirs.len(), 2);
        assert_eq!(
            cfg.extra_dirs[0],
            std::path::PathBuf::from("/custom/agents")
        );
    }

    #[test]
    fn subagent_config_partial_section_uses_field_defaults() {
        // Only max_concurrent provided — other fields use Default.
        let toml = r#"max_concurrent = 3"#;
        let cfg: SubAgentConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.max_concurrent, 3);
        assert!(!cfg.enabled);
        assert!(cfg.extra_dirs.is_empty());
    }

    #[test]
    fn subagent_config_default_permission_mode_is_none() {
        let cfg = SubAgentConfig::default();
        assert!(cfg.default_permission_mode.is_none());
        assert!(cfg.default_disallowed_tools.is_empty());
    }

    #[test]
    fn subagent_config_default_permission_mode_deserializes() {
        use crate::subagent::def::PermissionMode;
        let toml = r#"
            enabled = true
            max_concurrent = 2
            default_permission_mode = "plan"
            default_disallowed_tools = ["dangerous_tool", "other"]
        "#;
        let cfg: SubAgentConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.default_permission_mode, Some(PermissionMode::Plan));
        assert_eq!(cfg.default_disallowed_tools, ["dangerous_tool", "other"]);
    }

    #[test]
    fn detector_mode_default_is_regex() {
        assert_eq!(DetectorMode::default(), DetectorMode::Regex);
    }

    #[test]
    fn learning_config_default_detector_mode_is_regex() {
        let cfg = LearningConfig::default();
        assert_eq!(cfg.detector_mode, DetectorMode::Regex);
        assert!(cfg.judge_model.is_empty());
        assert!((cfg.judge_adaptive_low - 0.5).abs() < f32::EPSILON);
        assert!((cfg.judge_adaptive_high - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn learning_config_deserialize_judge_mode() {
        let toml = r#"
            enabled = true
            detector_mode = "judge"
            judge_model = "claude-sonnet-4-6"
            judge_adaptive_low = 0.4
            judge_adaptive_high = 0.9
        "#;
        let cfg: LearningConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.detector_mode, DetectorMode::Judge);
        assert_eq!(cfg.judge_model, "claude-sonnet-4-6");
        assert!((cfg.judge_adaptive_low - 0.4).abs() < f32::EPSILON);
        assert!((cfg.judge_adaptive_high - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn learning_config_detector_mode_defaults_to_regex_when_absent() {
        let toml = "enabled = true";
        let cfg: LearningConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.detector_mode, DetectorMode::Regex);
    }

    // --- RoutingConfig / CompressionConfig TOML deserialization tests (#1162, #1161) ---

    #[test]
    fn routing_strategy_default_is_heuristic() {
        assert_eq!(RoutingStrategy::default(), RoutingStrategy::Heuristic);
        let cfg = RoutingConfig::default();
        assert_eq!(cfg.strategy, RoutingStrategy::Heuristic);
    }

    #[test]
    fn routing_config_toml_heuristic() {
        let toml = r#"strategy = "heuristic""#;
        let cfg: RoutingConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.strategy, RoutingStrategy::Heuristic);
    }

    #[test]
    fn routing_config_toml_invalid_strategy_rejected() {
        let toml = r#"strategy = "unknown_strategy""#;
        let result: Result<RoutingConfig, _> = toml::from_str(toml);
        assert!(
            result.is_err(),
            "unknown strategy must fail deserialization"
        );
    }

    #[test]
    fn compression_strategy_default_is_reactive() {
        assert_eq!(
            CompressionStrategy::default(),
            CompressionStrategy::Reactive
        );
        let cfg = CompressionConfig::default();
        assert_eq!(cfg.strategy, CompressionStrategy::Reactive);
    }

    #[test]
    fn compression_config_toml_reactive() {
        let toml = r#"strategy = "reactive""#;
        let cfg: CompressionConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.strategy, CompressionStrategy::Reactive);
    }

    #[test]
    fn compression_config_toml_proactive() {
        let toml = r#"
            strategy = "proactive"
            threshold_tokens = 80000
            max_summary_tokens = 4000
        "#;
        let cfg: CompressionConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.strategy,
            CompressionStrategy::Proactive {
                threshold_tokens: 80_000,
                max_summary_tokens: 4_000,
            }
        );
    }

    #[test]
    fn compression_config_toml_model_roundtrip() {
        let toml = r#"
            strategy = "reactive"
            model = "claude-haiku-4-5-20251001"
        "#;
        let cfg: CompressionConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.model, "claude-haiku-4-5-20251001");
        let serialized = toml::to_string_pretty(&cfg).unwrap();
        let back: CompressionConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(back.model, cfg.model);
    }

    #[test]
    fn routing_config_toml_question_with_double_colon_routes_test() {
        // Verifies that "what does foo::bar do" routes Semantic, not Keyword.
        // This tests the question-word override for structural code patterns (router.rs:69-70).
        // The test lives here to keep router.rs unit-focused and types.rs integration-focused.
        use zeph_memory::{HeuristicRouter, MemoryRoute, MemoryRouter};
        let router = HeuristicRouter;
        assert_eq!(
            router.route("what does foo::bar do"),
            MemoryRoute::Semantic,
            "question word must override :: structural pattern"
        );
    }

    #[test]
    fn router_strategy_config_serde_ema() {
        let s: RouterStrategyConfig = toml::from_str("strategy = \"ema\"")
            .map(|t: toml::Value| {
                t["strategy"]
                    .as_str()
                    .and_then(|v| {
                        serde_json::from_value(serde_json::Value::String(v.to_owned())).ok()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        assert_eq!(s, RouterStrategyConfig::Ema);
        let json = serde_json::to_string(&RouterStrategyConfig::Ema).unwrap();
        assert_eq!(json, r#""ema""#);
    }

    #[test]
    fn router_strategy_config_serde_thompson() {
        let json = serde_json::to_string(&RouterStrategyConfig::Thompson).unwrap();
        assert_eq!(json, r#""thompson""#);
        let back: RouterStrategyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, RouterStrategyConfig::Thompson);
    }

    #[test]
    fn router_strategy_config_default_is_ema() {
        assert_eq!(RouterStrategyConfig::default(), RouterStrategyConfig::Ema);
    }

    #[test]
    fn router_strategy_config_invalid_deserialize_fails() {
        let result: Result<RouterStrategyConfig, _> = serde_json::from_str(r#""unknown""#);
        assert!(result.is_err(), "unknown variant must fail to deserialize");
    }

    #[test]
    fn graph_config_defaults() {
        let cfg = GraphConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.extract_model.is_empty());
        assert_eq!(cfg.max_entities_per_message, 10);
        assert_eq!(cfg.max_edges_per_message, 15);
        assert_eq!(cfg.community_refresh_interval, 100);
        assert!((cfg.entity_similarity_threshold - 0.85).abs() < f32::EPSILON);
        assert_eq!(cfg.extraction_timeout_secs, 15);
        assert!(!cfg.use_embedding_resolution);
        assert_eq!(cfg.max_hops, 2);
        assert_eq!(cfg.recall_limit, 10);
    }

    #[test]
    fn graph_config_toml_round_trip() {
        let original = GraphConfig::default();
        let toml_str = toml::to_string_pretty(&original).expect("serialize");
        let back: GraphConfig = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(back.enabled, original.enabled);
        assert_eq!(back.max_hops, original.max_hops);
        assert_eq!(back.recall_limit, original.recall_limit);
    }
}
