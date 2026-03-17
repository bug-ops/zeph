// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};
use zeph_llm::{GeminiThinkingLevel, ThinkingConfig};

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

fn default_gemini_max_tokens() -> u32 {
    8192
}

fn default_gemini_base_url() -> String {
    "https://generativelanguage.googleapis.com".to_owned()
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

fn default_cascade_quality_threshold() -> f64 {
    0.5
}

fn default_cascade_max_escalations() -> u8 {
    2
}

fn default_cascade_window_size() -> usize {
    50
}

#[must_use]
pub fn default_stt_provider() -> String {
    "whisper".into()
}

#[must_use]
pub fn default_stt_model() -> String {
    "whisper-1".into()
}

#[must_use]
pub fn default_stt_language() -> String {
    "auto".into()
}

pub(super) fn get_default_embedding_model() -> String {
    default_embedding_model()
}

pub(super) fn get_default_response_cache_ttl_secs() -> u64 {
    default_response_cache_ttl_secs()
}

pub(super) fn get_default_router_ema_alpha() -> f64 {
    default_router_ema_alpha()
}

pub(super) fn get_default_router_reorder_interval() -> u64 {
    default_router_reorder_interval()
}

/// LLM provider backend selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Ollama,
    Claude,
    OpenAi,
    Gemini,
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
            Self::Gemini => "gemini",
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gemini: Option<GeminiConfig>,
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
    /// Shorthand model spec for tool-pair summarization and context compaction.
    /// Format: `ollama/<model>`, `claude[/<model>]`, `openai[/<model>]`, `compatible/<name>`, `candle`.
    /// Ignored when `[llm.summary_provider]` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_model: Option<String>,
    /// Structured provider config for summarization. Takes precedence over `summary_model`.
    /// Same format as `[llm.orchestrator.providers.*]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_provider: Option<OrchestratorProviderConfig>,
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

#[derive(Debug, Deserialize, Serialize)]
pub struct CloudLlmConfig {
    pub model: String,
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    /// Enable server-side context compaction (Claude API compact-2026-01-12 beta).
    /// When true, the Claude API automatically summarizes long conversations.
    /// Client-side compaction is skipped when this is active.
    #[serde(default)]
    pub server_compaction: bool,
    /// Enable 1M token extended context window for Claude Opus 4.6 and Sonnet 4.6.
    /// When enabled, injects `anthropic-beta: context-1m-2025-08-07` header.
    /// NOTE: tokens above 200K use long-context pricing (see Anthropic docs).
    #[serde(default)]
    pub enable_extended_context: bool,
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
pub struct GeminiConfig {
    /// Gemini model name, e.g. `gemini-2.0-flash`.
    pub model: String,
    /// Maximum output tokens.
    #[serde(default = "default_gemini_max_tokens")]
    pub max_tokens: u32,
    /// API base URL. Default: `https://generativelanguage.googleapis.com`.
    /// Can be overridden for proxies or Vertex AI.
    #[serde(default = "default_gemini_base_url")]
    pub base_url: String,
    /// Embedding model name, e.g. `text-embedding-004`.
    /// When set, `supports_embeddings()` returns true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
    /// Thinking level for Gemini 3+ models: minimal, low, medium, high.
    /// For Gemini 2.5 models use `thinking_budget` instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<GeminiThinkingLevel>,
    /// Thinking token budget for Gemini 2.5 models (0 = disable, -1 = dynamic, 0–32768).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<i32>,
    /// Include thinking summaries in the response (default: false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_thoughts: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CompatibleConfig {
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
    #[serde(default)]
    pub embedding_model: Option<String>,
    /// Optional API key set directly in config. When absent, falls back to
    /// `ZEPH_COMPATIBLE_<NAME>_API_KEY` vault secret. For local endpoints
    /// (localhost / private networks) neither is required.
    #[serde(default)]
    pub api_key: Option<String>,
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
    /// Cascade routing: try cheapest provider first, escalate on degenerate output.
    Cascade,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RouterConfig {
    /// Ordered list of provider names to route across. Cost order for cascade: cheapest first.
    pub chain: Vec<String>,
    /// Routing strategy: `"ema"` (default), `"thompson"`, or `"cascade"`.
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
    /// Cascade routing configuration. Only used when `strategy = "cascade"`.
    #[serde(default)]
    pub cascade: Option<CascadeConfig>,
}

/// Configuration for cascade routing (`strategy = "cascade"`).
///
/// Cascade routing tries providers in chain order (cheapest first), escalating to
/// the next provider when the response is classified as degenerate (empty, repetitive,
/// incoherent). Chain order determines cost order: first provider = cheapest.
///
/// # Limitations
///
/// The heuristic classifier detects degenerate outputs only, not semantic failures.
/// Use `classifier_mode = "judge"` for semantic quality gating (adds LLM call cost).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CascadeConfig {
    /// Minimum quality score [0.0, 1.0] to accept a response without escalating.
    /// Responses scoring below this threshold trigger escalation.
    #[serde(default = "default_cascade_quality_threshold")]
    pub quality_threshold: f64,

    /// Maximum number of quality-based escalations per request.
    /// Network/API errors do not count against this budget.
    /// Default: 2 (allows up to 3 providers: cheap → mid → expensive).
    #[serde(default = "default_cascade_max_escalations")]
    pub max_escalations: u8,

    /// Quality classifier mode: `"heuristic"` (default) or `"judge"`.
    /// Heuristic is zero-cost but detects only degenerate outputs.
    /// Judge requires a configured `summary_model` and adds one LLM call per evaluation.
    #[serde(default)]
    pub classifier_mode: CascadeClassifierMode,

    /// Rolling quality history window size per provider. Default: 50.
    #[serde(default = "default_cascade_window_size")]
    pub window_size: usize,

    /// Maximum cumulative input+output tokens across all escalation levels.
    /// When exceeded, returns the best-seen response instead of escalating further.
    /// `None` disables the budget (unbounded escalation cost).
    #[serde(default)]
    pub max_cascade_tokens: Option<u32>,

    /// Explicit cost ordering of provider names (cheapest first).
    /// When set, cascade routing sorts providers by their position in this list before
    /// trying them. Providers not in the list are appended after listed ones in their
    /// original chain order. When unset, chain order is used (default behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_tiers: Option<Vec<String>>,
}

impl Default for CascadeConfig {
    fn default() -> Self {
        Self {
            quality_threshold: default_cascade_quality_threshold(),
            max_escalations: default_cascade_max_escalations(),
            classifier_mode: CascadeClassifierMode::default(),
            window_size: default_cascade_window_size(),
            max_cascade_tokens: None,
            cost_tiers: None,
        }
    }
}

/// Quality classifier mode for cascade routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CascadeClassifierMode {
    /// Zero-cost heuristic: detects degenerate outputs (empty, repetitive, incoherent).
    /// Does not detect semantic failures (hallucinations, wrong answers).
    #[default]
    Heuristic,
    /// LLM-based judge: more accurate but adds latency. Falls back to heuristic on failure.
    /// Requires `summary_model` to be configured.
    Judge,
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

pub const MAX_TOKENS_CAP: usize = 32768;

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

#[derive(Debug, Deserialize, Serialize)]
pub struct OrchestratorConfig {
    pub default: String,
    pub embed: String,
    #[serde(default)]
    pub providers: std::collections::HashMap<String, OrchestratorProviderConfig>,
    #[serde(default)]
    pub routes: std::collections::HashMap<String, Vec<String>>,
    /// How long (in seconds) a failed provider is bypassed before being retried.
    /// Defaults to 300 seconds (5 minutes) when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_ttl_secs: Option<u64>,
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
