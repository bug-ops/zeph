// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};
use zeph_llm::{GeminiThinkingLevel, ThinkingConfig};

fn default_response_cache_ttl_secs() -> u64 {
    3600
}

fn default_semantic_cache_threshold() -> f32 {
    0.95
}

fn default_semantic_cache_max_candidates() -> u32 {
    10
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

fn default_reputation_decay_factor() -> f64 {
    0.95
}

fn default_reputation_weight() -> f64 {
    0.3
}

fn default_reputation_min_observations() -> u64 {
    5
}

#[must_use]
pub fn default_stt_provider() -> String {
    String::new()
}

#[must_use]
pub fn default_stt_language() -> String {
    "auto".into()
}

#[must_use]
pub fn get_default_embedding_model() -> String {
    default_embedding_model()
}

#[must_use]
pub fn get_default_response_cache_ttl_secs() -> u64 {
    default_response_cache_ttl_secs()
}

#[must_use]
pub fn get_default_router_ema_alpha() -> f64 {
    default_router_ema_alpha()
}

#[must_use]
pub fn get_default_router_reorder_interval() -> u64 {
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
    Compatible,
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
            Self::Compatible => "compatible",
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
    /// Provider pool. First entry is default unless one is marked `default = true`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderEntry>,

    /// Routing strategy for multi-provider configs.
    #[serde(default, skip_serializing_if = "is_routing_none")]
    pub routing: LlmRoutingStrategy,

    /// Task-based routes (only used when `routing = "task"`).
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub routes: std::collections::HashMap<String, Vec<String>>,

    #[serde(default = "default_embedding_model_opt")]
    pub embedding_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candle: Option<CandleConfig>,
    #[serde(default)]
    pub stt: Option<SttConfig>,
    #[serde(default)]
    pub response_cache_enabled: bool,
    #[serde(default = "default_response_cache_ttl_secs")]
    pub response_cache_ttl_secs: u64,
    /// Enable semantic similarity-based response caching. Requires embedding support.
    #[serde(default)]
    pub semantic_cache_enabled: bool,
    /// Cosine similarity threshold for semantic cache hits (0.0–1.0).
    ///
    /// Only the highest-scoring candidate above this threshold is returned.
    /// Lower values produce more cache hits but risk returning less relevant responses.
    /// Recommended range: 0.92–0.98; default: 0.95.
    #[serde(default = "default_semantic_cache_threshold")]
    pub semantic_cache_threshold: f32,
    /// Maximum cached entries to examine per semantic lookup (SQL `LIMIT` clause in
    /// `ResponseCache::get_semantic()`). Controls the recall-vs-performance tradeoff:
    ///
    /// - **Higher values** (e.g. 50): scan more entries, better chance of finding a
    ///   semantically similar cached response, but slower queries.
    /// - **Lower values** (e.g. 5): faster queries, but may miss relevant cached entries
    ///   when the cache is large.
    /// - **Default (10)**: balanced middle ground for typical workloads.
    ///
    /// Tuning guidance: set to 50+ when recall matters more than latency (e.g. long-running
    /// sessions with many cached responses); reduce to 5 for low-latency interactive use.
    /// Env override: `ZEPH_LLM_SEMANTIC_CACHE_MAX_CANDIDATES`.
    #[serde(default = "default_semantic_cache_max_candidates")]
    pub semantic_cache_max_candidates: u32,
    #[serde(default)]
    pub router_ema_enabled: bool,
    #[serde(default = "default_router_ema_alpha")]
    pub router_ema_alpha: f64,
    #[serde(default = "default_router_reorder_interval")]
    pub router_reorder_interval: u64,
    /// Routing configuration for Thompson/Cascade strategies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub router: Option<RouterConfig>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_provider: Option<ProviderEntry>,

    /// Complexity triage routing configuration. Required when `routing = "triage"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complexity_routing: Option<ComplexityRoutingConfig>,
}

fn default_embedding_model_opt() -> String {
    default_embedding_model()
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_routing_none(s: &LlmRoutingStrategy) -> bool {
    *s == LlmRoutingStrategy::None
}

impl LlmConfig {
    /// Effective provider kind for the primary (first/default) provider in the pool.
    #[must_use]
    pub fn effective_provider(&self) -> ProviderKind {
        self.providers
            .first()
            .map_or(ProviderKind::Ollama, |e| e.provider_type)
    }

    /// Effective base URL for the primary provider.
    #[must_use]
    pub fn effective_base_url(&self) -> &str {
        self.providers
            .first()
            .and_then(|e| e.base_url.as_deref())
            .unwrap_or("http://localhost:11434")
    }

    /// Effective model for the primary provider.
    #[must_use]
    pub fn effective_model(&self) -> &str {
        self.providers
            .first()
            .and_then(|e| e.model.as_deref())
            .unwrap_or("qwen3:8b")
    }

    /// Find the provider entry designated for STT.
    ///
    /// Resolution priority:
    /// 1. `[llm.stt].provider` matches `[[llm.providers]].name` and the entry has `stt_model`
    /// 2. `[llm.stt].provider` is empty — fall through to auto-detect
    /// 3. First provider with `stt_model` set (auto-detect fallback)
    /// 4. `None` — STT disabled
    #[must_use]
    pub fn stt_provider_entry(&self) -> Option<&ProviderEntry> {
        let name_hint = self.stt.as_ref().map_or("", |s| s.provider.as_str());
        if name_hint.is_empty() {
            self.providers.iter().find(|p| p.stt_model.is_some())
        } else {
            self.providers
                .iter()
                .find(|p| p.effective_name() == name_hint && p.stt_model.is_some())
        }
    }

    /// Validate that the config uses the new `[[llm.providers]]` format.
    ///
    /// # Errors
    ///
    /// Returns `ConfigError::Validation` when no providers are configured.
    pub fn check_legacy_format(&self) -> Result<(), crate::error::ConfigError> {
        Ok(())
    }

    /// Validate STT config cross-references.
    ///
    /// # Errors
    ///
    /// Returns `ConfigError::Validation` when the referenced STT provider does not exist.
    pub fn validate_stt(&self) -> Result<(), crate::error::ConfigError> {
        use crate::error::ConfigError;

        let Some(stt) = &self.stt else {
            return Ok(());
        };
        if stt.provider.is_empty() {
            return Ok(());
        }
        let found = self
            .providers
            .iter()
            .find(|p| p.effective_name() == stt.provider);
        match found {
            None => {
                return Err(ConfigError::Validation(format!(
                    "[llm.stt].provider = {:?} does not match any [[llm.providers]] entry",
                    stt.provider
                )));
            }
            Some(entry) if entry.stt_model.is_none() => {
                tracing::warn!(
                    provider = stt.provider,
                    "[[llm.providers]] entry exists but has no `stt_model` — STT will not be activated"
                );
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SttConfig {
    /// Provider name from `[[llm.providers]]`. Empty string means auto-detect first provider
    /// with `stt_model` set.
    #[serde(default = "default_stt_provider")]
    pub provider: String,
    /// Language hint for transcription (e.g. `"en"`, `"auto"`).
    #[serde(default = "default_stt_language")]
    pub language: String,
}

/// Routing strategy selection for multi-provider routing.
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

/// Routing configuration for multi-provider setups.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RouterConfig {
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
    /// Bayesian reputation scoring configuration (RAPS). Disabled by default.
    #[serde(default)]
    pub reputation: Option<ReputationConfig>,
}

/// Configuration for Bayesian reputation scoring (RAPS — Reputation-Adjusted Provider Selection).
///
/// When enabled, quality outcomes from tool execution shift the routing scores over time,
/// giving an advantage to providers that consistently produce valid tool arguments.
///
/// Default: disabled. Set `enabled = true` to activate.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReputationConfig {
    /// Enable reputation scoring. Default: false.
    #[serde(default)]
    pub enabled: bool,
    /// Session-level decay factor applied on each load. Range: (0.0, 1.0]. Default: 0.95.
    /// Lower values make reputation forget faster; 1.0 = no decay.
    #[serde(default = "default_reputation_decay_factor")]
    pub decay_factor: f64,
    /// Weight of reputation in routing score blend. Range: [0.0, 1.0]. Default: 0.3.
    ///
    /// **Warning**: values above 0.5 can aggressively suppress low-reputation providers.
    /// At `weight = 1.0` with `rep_factor = 0.0` (all failures), the routing score
    /// drops to zero — the provider becomes unreachable for that session. Stick to
    /// the default (0.3) unless you intentionally want strong reputation gating.
    #[serde(default = "default_reputation_weight")]
    pub weight: f64,
    /// Minimum quality observations before reputation influences routing. Default: 5.
    #[serde(default = "default_reputation_min_observations")]
    pub min_observations: u64,
    /// Path for persisting reputation state. Defaults to `~/.config/zeph/router_reputation_state.json`.
    #[serde(default)]
    pub state_path: Option<String>,
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
    /// Resolved `HuggingFace` Hub API token for authenticated model downloads.
    ///
    /// Must be the **token value** — resolved by the caller before constructing this config.
    #[serde(default)]
    pub hf_token: Option<String>,
    #[serde(default)]
    pub generation: GenerationParams,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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

// ─── Unified config types ─────────────────────────────────────────────────────

/// Routing strategy for the `[[llm.providers]]` pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LlmRoutingStrategy {
    /// Single provider or first-in-pool (default).
    #[default]
    None,
    /// Exponential moving average latency-aware ordering.
    Ema,
    /// Thompson Sampling with Beta distributions.
    Thompson,
    /// Cascade: try cheapest provider first, escalate on degenerate output.
    Cascade,
    /// Task-based routing using `[llm.routes]` map.
    Task,
    /// Complexity triage routing: pre-classify each request, delegate to appropriate tier.
    Triage,
}

fn default_triage_timeout_secs() -> u64 {
    5
}

fn default_max_triage_tokens() -> u32 {
    50
}

fn default_true() -> bool {
    true
}

/// Tier-to-provider name mapping for complexity routing.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TierMapping {
    pub simple: Option<String>,
    pub medium: Option<String>,
    pub complex: Option<String>,
    pub expert: Option<String>,
}

/// Configuration for complexity-based triage routing (`routing = "triage"`).
///
/// When `[llm] routing = "triage"` is set, a cheap triage model classifies each request
/// and routes it to the appropriate tier provider. Requires at least one tier mapping.
///
/// # Example
///
/// ```toml
/// [llm]
/// routing = "triage"
///
/// [llm.complexity_routing]
/// triage_provider = "local-fast"
///
/// [llm.complexity_routing.tiers]
/// simple = "local-fast"
/// medium = "haiku"
/// complex = "sonnet"
/// expert = "opus"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ComplexityRoutingConfig {
    /// Provider name from `[[llm.providers]]` used for triage classification.
    #[serde(default)]
    pub triage_provider: Option<String>,

    /// Skip triage when all tiers map to the same provider.
    #[serde(default = "default_true")]
    pub bypass_single_provider: bool,

    /// Tier-to-provider name mapping.
    #[serde(default)]
    pub tiers: TierMapping,

    /// Max output tokens for the triage classification call. Default: 50.
    #[serde(default = "default_max_triage_tokens")]
    pub max_triage_tokens: u32,

    /// Timeout in seconds for the triage classification call. Default: 5.
    /// On timeout, falls back to the default (first) tier provider.
    #[serde(default = "default_triage_timeout_secs")]
    pub triage_timeout_secs: u64,

    /// Optional fallback strategy when triage misclassifies.
    /// Only `"cascade"` is currently supported (Phase 4).
    #[serde(default)]
    pub fallback_strategy: Option<String>,
}

impl Default for ComplexityRoutingConfig {
    fn default() -> Self {
        Self {
            triage_provider: None,
            bypass_single_provider: true,
            tiers: TierMapping::default(),
            max_triage_tokens: default_max_triage_tokens(),
            triage_timeout_secs: default_triage_timeout_secs(),
            fallback_strategy: None,
        }
    }
}

/// Inline candle config for use inside `ProviderEntry`.
/// Re-uses the generation params from `CandleConfig`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CandleInlineConfig {
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
    /// Resolved `HuggingFace` Hub API token for authenticated model downloads.
    #[serde(default)]
    pub hf_token: Option<String>,
    #[serde(default)]
    pub generation: GenerationParams,
}

impl Default for CandleInlineConfig {
    fn default() -> Self {
        Self {
            source: default_candle_source(),
            local_path: String::new(),
            filename: None,
            chat_template: default_chat_template(),
            device: default_candle_device(),
            embedding_repo: None,
            hf_token: None,
            generation: GenerationParams::default(),
        }
    }
}

/// Unified provider entry: one struct replaces `CloudLlmConfig`, `OpenAiConfig`,
/// `GeminiConfig`, `OllamaConfig`, `CompatibleConfig`, and `OrchestratorProviderConfig`.
///
/// Provider-specific fields use `#[serde(default)]` and are ignored by backends
/// that do not use them (flat-union pattern).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ProviderEntry {
    /// Required: provider backend type.
    #[serde(rename = "type")]
    pub provider_type: ProviderKind,

    /// Optional name for multi-provider configs. Auto-generated from type if absent.
    #[serde(default)]
    pub name: Option<String>,

    /// Model identifier. Required for most types.
    #[serde(default)]
    pub model: Option<String>,

    /// API base URL. Each type has its own default.
    #[serde(default)]
    pub base_url: Option<String>,

    /// Max output tokens.
    #[serde(default)]
    pub max_tokens: Option<u32>,

    /// Embedding model. When set, this provider supports `embed()` calls.
    #[serde(default)]
    pub embedding_model: Option<String>,

    /// STT model. When set, this provider supports speech-to-text via the Whisper API or
    /// Candle-local inference.
    #[serde(default)]
    pub stt_model: Option<String>,

    /// Mark this entry as the embedding provider (handles `embed()` calls).
    #[serde(default)]
    pub embed: bool,

    /// Mark this entry as the default chat provider (overrides position-based default).
    #[serde(default)]
    pub default: bool,

    // --- Claude-specific ---
    #[serde(default)]
    pub thinking: Option<ThinkingConfig>,
    #[serde(default)]
    pub server_compaction: bool,
    #[serde(default)]
    pub enable_extended_context: bool,

    // --- OpenAI-specific ---
    #[serde(default)]
    pub reasoning_effort: Option<String>,

    // --- Gemini-specific ---
    #[serde(default)]
    pub thinking_level: Option<GeminiThinkingLevel>,
    #[serde(default)]
    pub thinking_budget: Option<i32>,
    #[serde(default)]
    pub include_thoughts: Option<bool>,

    // --- Ollama-specific ---
    #[serde(default)]
    pub tool_use: bool,

    // --- Compatible-specific: optional inline api_key ---
    #[serde(default)]
    pub api_key: Option<String>,

    // --- Candle-specific ---
    #[serde(default)]
    pub candle: Option<CandleInlineConfig>,

    // --- Vision ---
    #[serde(default)]
    pub vision_model: Option<String>,

    /// Provider-specific instruction file.
    #[serde(default)]
    pub instruction_file: Option<std::path::PathBuf>,
}

impl Default for ProviderEntry {
    fn default() -> Self {
        Self {
            provider_type: ProviderKind::Ollama,
            name: None,
            model: None,
            base_url: None,
            max_tokens: None,
            embedding_model: None,
            stt_model: None,
            embed: false,
            default: false,
            thinking: None,
            server_compaction: false,
            enable_extended_context: false,
            reasoning_effort: None,
            thinking_level: None,
            thinking_budget: None,
            include_thoughts: None,
            tool_use: false,
            api_key: None,
            candle: None,
            vision_model: None,
            instruction_file: None,
        }
    }
}

impl ProviderEntry {
    /// Resolve the effective name: explicit `name` field or type string.
    #[must_use]
    pub fn effective_name(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| self.provider_type.as_str().to_owned())
    }

    /// Resolve the effective model: explicit `model` field or the provider-type default.
    ///
    /// Defaults mirror those used in `build_provider_from_entry` so that `runtime.model_name`
    /// always reflects the actual model being used rather than the provider type string.
    #[must_use]
    pub fn effective_model(&self) -> String {
        if let Some(ref m) = self.model {
            return m.clone();
        }
        match self.provider_type {
            ProviderKind::Ollama => "qwen3:8b".to_owned(),
            ProviderKind::Claude => "claude-haiku-4-5-20251001".to_owned(),
            ProviderKind::OpenAi => "gpt-4o-mini".to_owned(),
            ProviderKind::Gemini => "gemini-2.0-flash".to_owned(),
            ProviderKind::Compatible | ProviderKind::Candle => String::new(),
        }
    }

    /// Validate this entry for cross-field consistency.
    ///
    /// # Errors
    ///
    /// Returns `ConfigError` when a fatal invariant is violated (e.g. compatible provider
    /// without a name).
    pub fn validate(&self) -> Result<(), crate::error::ConfigError> {
        use crate::error::ConfigError;

        // B2: compatible provider MUST have name set.
        if self.provider_type == ProviderKind::Compatible && self.name.is_none() {
            return Err(ConfigError::Validation(
                "[[llm.providers]] entry with type=\"compatible\" must set `name`".into(),
            ));
        }

        // B1: warn on irrelevant fields.
        match self.provider_type {
            ProviderKind::Ollama => {
                if self.thinking.is_some() {
                    tracing::warn!(
                        provider = self.effective_name(),
                        "field `thinking` is only used by Claude providers"
                    );
                }
                if self.reasoning_effort.is_some() {
                    tracing::warn!(
                        provider = self.effective_name(),
                        "field `reasoning_effort` is only used by OpenAI providers"
                    );
                }
                if self.thinking_level.is_some() || self.thinking_budget.is_some() {
                    tracing::warn!(
                        provider = self.effective_name(),
                        "fields `thinking_level`/`thinking_budget` are only used by Gemini providers"
                    );
                }
            }
            ProviderKind::Claude => {
                if self.reasoning_effort.is_some() {
                    tracing::warn!(
                        provider = self.effective_name(),
                        "field `reasoning_effort` is only used by OpenAI providers"
                    );
                }
                if self.thinking_level.is_some() || self.thinking_budget.is_some() {
                    tracing::warn!(
                        provider = self.effective_name(),
                        "fields `thinking_level`/`thinking_budget` are only used by Gemini providers"
                    );
                }
                if self.tool_use {
                    tracing::warn!(
                        provider = self.effective_name(),
                        "field `tool_use` is only used by Ollama providers"
                    );
                }
            }
            ProviderKind::OpenAi => {
                if self.thinking.is_some() {
                    tracing::warn!(
                        provider = self.effective_name(),
                        "field `thinking` is only used by Claude providers"
                    );
                }
                if self.thinking_level.is_some() || self.thinking_budget.is_some() {
                    tracing::warn!(
                        provider = self.effective_name(),
                        "fields `thinking_level`/`thinking_budget` are only used by Gemini providers"
                    );
                }
                if self.tool_use {
                    tracing::warn!(
                        provider = self.effective_name(),
                        "field `tool_use` is only used by Ollama providers"
                    );
                }
            }
            ProviderKind::Gemini => {
                if self.thinking.is_some() {
                    tracing::warn!(
                        provider = self.effective_name(),
                        "field `thinking` is only used by Claude providers"
                    );
                }
                if self.reasoning_effort.is_some() {
                    tracing::warn!(
                        provider = self.effective_name(),
                        "field `reasoning_effort` is only used by OpenAI providers"
                    );
                }
                if self.tool_use {
                    tracing::warn!(
                        provider = self.effective_name(),
                        "field `tool_use` is only used by Ollama providers"
                    );
                }
            }
            _ => {}
        }

        // W6: Candle STT-only provider (stt_model set, no model) is valid — no warning needed.
        // Warn if Ollama has stt_model set (Ollama does not support Whisper API).
        if self.stt_model.is_some() && self.provider_type == ProviderKind::Ollama {
            tracing::warn!(
                provider = self.effective_name(),
                "field `stt_model` is set on an Ollama provider; Ollama does not support the \
                 Whisper STT API — use OpenAI, compatible, or candle instead"
            );
        }

        Ok(())
    }
}

/// Validate a pool of `ProviderEntry` items.
///
/// # Errors
///
/// Returns `ConfigError` for fatal validation failures:
/// - Empty pool
/// - Duplicate names
/// - Multiple entries marked `default = true`
/// - Individual entry validation errors
pub fn validate_pool(entries: &[ProviderEntry]) -> Result<(), crate::error::ConfigError> {
    use crate::error::ConfigError;
    use std::collections::HashSet;

    if entries.is_empty() {
        return Err(ConfigError::Validation(
            "at least one LLM provider must be configured in [[llm.providers]]".into(),
        ));
    }

    let default_count = entries.iter().filter(|e| e.default).count();
    if default_count > 1 {
        return Err(ConfigError::Validation(
            "only one [[llm.providers]] entry can be marked `default = true`".into(),
        ));
    }

    let mut seen_names: HashSet<String> = HashSet::new();
    for entry in entries {
        let name = entry.effective_name();
        if !seen_names.insert(name.clone()) {
            return Err(ConfigError::Validation(format!(
                "duplicate provider name \"{name}\" in [[llm.providers]]"
            )));
        }
        entry.validate()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ollama_entry() -> ProviderEntry {
        ProviderEntry {
            provider_type: ProviderKind::Ollama,
            name: Some("ollama".into()),
            model: Some("qwen3:8b".into()),
            ..Default::default()
        }
    }

    fn claude_entry() -> ProviderEntry {
        ProviderEntry {
            provider_type: ProviderKind::Claude,
            name: Some("claude".into()),
            model: Some("claude-sonnet-4-6".into()),
            max_tokens: Some(8192),
            ..Default::default()
        }
    }

    // ─── ProviderEntry::validate ─────────────────────────────────────────────

    #[test]
    fn validate_ollama_valid() {
        assert!(ollama_entry().validate().is_ok());
    }

    #[test]
    fn validate_claude_valid() {
        assert!(claude_entry().validate().is_ok());
    }

    #[test]
    fn validate_compatible_without_name_errors() {
        let entry = ProviderEntry {
            provider_type: ProviderKind::Compatible,
            name: None,
            ..Default::default()
        };
        let err = entry.validate().unwrap_err();
        assert!(
            err.to_string().contains("compatible"),
            "error should mention compatible: {err}"
        );
    }

    #[test]
    fn validate_compatible_with_name_ok() {
        let entry = ProviderEntry {
            provider_type: ProviderKind::Compatible,
            name: Some("my-proxy".into()),
            base_url: Some("http://localhost:8080".into()),
            model: Some("gpt-4o".into()),
            max_tokens: Some(4096),
            ..Default::default()
        };
        assert!(entry.validate().is_ok());
    }

    #[test]
    fn validate_openai_valid() {
        let entry = ProviderEntry {
            provider_type: ProviderKind::OpenAi,
            name: Some("openai".into()),
            model: Some("gpt-4o".into()),
            max_tokens: Some(4096),
            ..Default::default()
        };
        assert!(entry.validate().is_ok());
    }

    #[test]
    fn validate_gemini_valid() {
        let entry = ProviderEntry {
            provider_type: ProviderKind::Gemini,
            name: Some("gemini".into()),
            model: Some("gemini-2.0-flash".into()),
            ..Default::default()
        };
        assert!(entry.validate().is_ok());
    }

    // ─── validate_pool ───────────────────────────────────────────────────────

    #[test]
    fn validate_pool_empty_errors() {
        let err = validate_pool(&[]).unwrap_err();
        assert!(err.to_string().contains("at least one"), "{err}");
    }

    #[test]
    fn validate_pool_single_entry_ok() {
        assert!(validate_pool(&[ollama_entry()]).is_ok());
    }

    #[test]
    fn validate_pool_duplicate_names_errors() {
        let a = ollama_entry();
        let b = ollama_entry(); // same effective name "ollama"
        let err = validate_pool(&[a, b]).unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn validate_pool_multiple_defaults_errors() {
        let mut a = ollama_entry();
        let mut b = claude_entry();
        a.default = true;
        b.default = true;
        let err = validate_pool(&[a, b]).unwrap_err();
        assert!(err.to_string().contains("default"), "{err}");
    }

    #[test]
    fn validate_pool_two_different_providers_ok() {
        assert!(validate_pool(&[ollama_entry(), claude_entry()]).is_ok());
    }

    #[test]
    fn validate_pool_propagates_entry_error() {
        let bad = ProviderEntry {
            provider_type: ProviderKind::Compatible,
            name: None, // invalid: compatible without name
            ..Default::default()
        };
        assert!(validate_pool(&[bad]).is_err());
    }

    // ─── ProviderEntry::effective_model ──────────────────────────────────────

    #[test]
    fn effective_model_returns_explicit_when_set() {
        let entry = ProviderEntry {
            provider_type: ProviderKind::Claude,
            model: Some("claude-sonnet-4-6".into()),
            ..Default::default()
        };
        assert_eq!(entry.effective_model(), "claude-sonnet-4-6");
    }

    #[test]
    fn effective_model_ollama_default_when_none() {
        let entry = ProviderEntry {
            provider_type: ProviderKind::Ollama,
            model: None,
            ..Default::default()
        };
        assert_eq!(entry.effective_model(), "qwen3:8b");
    }

    #[test]
    fn effective_model_claude_default_when_none() {
        let entry = ProviderEntry {
            provider_type: ProviderKind::Claude,
            model: None,
            ..Default::default()
        };
        assert_eq!(entry.effective_model(), "claude-haiku-4-5-20251001");
    }

    #[test]
    fn effective_model_openai_default_when_none() {
        let entry = ProviderEntry {
            provider_type: ProviderKind::OpenAi,
            model: None,
            ..Default::default()
        };
        assert_eq!(entry.effective_model(), "gpt-4o-mini");
    }

    #[test]
    fn effective_model_gemini_default_when_none() {
        let entry = ProviderEntry {
            provider_type: ProviderKind::Gemini,
            model: None,
            ..Default::default()
        };
        assert_eq!(entry.effective_model(), "gemini-2.0-flash");
    }

    // ─── LlmConfig::check_legacy_format ──────────────────────────────────────

    // Parse a complete TOML snippet that includes the [llm] header.
    fn parse_llm(toml: &str) -> LlmConfig {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            llm: LlmConfig,
        }
        toml::from_str::<Wrapper>(toml).unwrap().llm
    }

    #[test]
    fn check_legacy_format_new_format_ok() {
        let cfg = parse_llm(
            r#"
[llm]

[[llm.providers]]
type = "ollama"
model = "qwen3:8b"
"#,
        );
        assert!(cfg.check_legacy_format().is_ok());
    }

    #[test]
    fn check_legacy_format_empty_providers_no_legacy_ok() {
        // No providers, no legacy fields — passes (empty [llm] is acceptable here)
        let cfg = parse_llm("[llm]\n");
        assert!(cfg.check_legacy_format().is_ok());
    }

    // ─── LlmConfig::effective_* helpers ──────────────────────────────────────

    #[test]
    fn effective_provider_falls_back_to_ollama_when_no_providers() {
        let cfg = parse_llm("[llm]\n");
        assert_eq!(cfg.effective_provider(), ProviderKind::Ollama);
    }

    #[test]
    fn effective_provider_reads_from_providers_first() {
        let cfg = parse_llm(
            r#"
[llm]

[[llm.providers]]
type = "claude"
model = "claude-sonnet-4-6"
"#,
        );
        assert_eq!(cfg.effective_provider(), ProviderKind::Claude);
    }

    #[test]
    fn effective_model_reads_from_providers_first() {
        let cfg = parse_llm(
            r#"
[llm]

[[llm.providers]]
type = "ollama"
model = "qwen3:8b"
"#,
        );
        assert_eq!(cfg.effective_model(), "qwen3:8b");
    }

    #[test]
    fn effective_base_url_default_when_absent() {
        let cfg = parse_llm("[llm]\n");
        assert_eq!(cfg.effective_base_url(), "http://localhost:11434");
    }

    #[test]
    fn effective_base_url_from_providers_entry() {
        let cfg = parse_llm(
            r#"
[llm]

[[llm.providers]]
type = "ollama"
base_url = "http://myhost:11434"
"#,
        );
        assert_eq!(cfg.effective_base_url(), "http://myhost:11434");
    }

    // ─── ComplexityRoutingConfig / LlmRoutingStrategy::Triage TOML parsing ──

    #[test]
    fn complexity_routing_defaults() {
        let cr = ComplexityRoutingConfig::default();
        assert!(
            cr.bypass_single_provider,
            "bypass_single_provider must default to true"
        );
        assert_eq!(cr.triage_timeout_secs, 5);
        assert_eq!(cr.max_triage_tokens, 50);
        assert!(cr.triage_provider.is_none());
        assert!(cr.tiers.simple.is_none());
    }

    #[test]
    fn complexity_routing_toml_round_trip() {
        let cfg = parse_llm(
            r#"
[llm]
routing = "triage"

[llm.complexity_routing]
triage_provider = "fast"
bypass_single_provider = false
triage_timeout_secs = 10
max_triage_tokens = 100

[llm.complexity_routing.tiers]
simple = "fast"
medium = "medium"
complex = "large"
expert = "opus"
"#,
        );
        assert!(matches!(cfg.routing, LlmRoutingStrategy::Triage));
        let cr = cfg
            .complexity_routing
            .expect("complexity_routing must be present");
        assert_eq!(cr.triage_provider.as_deref(), Some("fast"));
        assert!(!cr.bypass_single_provider);
        assert_eq!(cr.triage_timeout_secs, 10);
        assert_eq!(cr.max_triage_tokens, 100);
        assert_eq!(cr.tiers.simple.as_deref(), Some("fast"));
        assert_eq!(cr.tiers.medium.as_deref(), Some("medium"));
        assert_eq!(cr.tiers.complex.as_deref(), Some("large"));
        assert_eq!(cr.tiers.expert.as_deref(), Some("opus"));
    }

    #[test]
    fn complexity_routing_partial_tiers_toml() {
        // Only simple + complex configured; medium and expert are None.
        let cfg = parse_llm(
            r#"
[llm]
routing = "triage"

[llm.complexity_routing.tiers]
simple = "haiku"
complex = "sonnet"
"#,
        );
        let cr = cfg
            .complexity_routing
            .expect("complexity_routing must be present");
        assert_eq!(cr.tiers.simple.as_deref(), Some("haiku"));
        assert!(cr.tiers.medium.is_none());
        assert_eq!(cr.tiers.complex.as_deref(), Some("sonnet"));
        assert!(cr.tiers.expert.is_none());
        // Defaults still applied.
        assert!(cr.bypass_single_provider);
        assert_eq!(cr.triage_timeout_secs, 5);
    }

    #[test]
    fn routing_strategy_triage_deserialized() {
        let cfg = parse_llm(
            r#"
[llm]
routing = "triage"
"#,
        );
        assert!(matches!(cfg.routing, LlmRoutingStrategy::Triage));
    }

    // ─── stt_provider_entry ───────────────────────────────────────────────────

    #[test]
    fn stt_provider_entry_by_name_match() {
        let cfg = parse_llm(
            r#"
[llm]

[[llm.providers]]
type = "openai"
name = "quality"
model = "gpt-5.4"
stt_model = "gpt-4o-mini-transcribe"

[llm.stt]
provider = "quality"
"#,
        );
        let entry = cfg.stt_provider_entry().expect("should find stt provider");
        assert_eq!(entry.effective_name(), "quality");
        assert_eq!(entry.stt_model.as_deref(), Some("gpt-4o-mini-transcribe"));
    }

    #[test]
    fn stt_provider_entry_auto_detect_when_provider_empty() {
        let cfg = parse_llm(
            r#"
[llm]

[[llm.providers]]
type = "openai"
name = "openai-stt"
stt_model = "whisper-1"

[llm.stt]
provider = ""
"#,
        );
        let entry = cfg.stt_provider_entry().expect("should auto-detect");
        assert_eq!(entry.effective_name(), "openai-stt");
    }

    #[test]
    fn stt_provider_entry_auto_detect_no_stt_section() {
        let cfg = parse_llm(
            r#"
[llm]

[[llm.providers]]
type = "openai"
name = "openai-stt"
stt_model = "whisper-1"
"#,
        );
        // No [llm.stt] section — should still find first provider with stt_model.
        let entry = cfg.stt_provider_entry().expect("should auto-detect");
        assert_eq!(entry.effective_name(), "openai-stt");
    }

    #[test]
    fn stt_provider_entry_none_when_no_stt_model() {
        let cfg = parse_llm(
            r#"
[llm]

[[llm.providers]]
type = "openai"
name = "quality"
model = "gpt-5.4"
"#,
        );
        assert!(cfg.stt_provider_entry().is_none());
    }

    #[test]
    fn stt_provider_entry_name_mismatch_falls_back_to_none() {
        // Named provider exists but has no stt_model; another unnamed has stt_model.
        let cfg = parse_llm(
            r#"
[llm]

[[llm.providers]]
type = "openai"
name = "quality"
model = "gpt-5.4"

[[llm.providers]]
type = "openai"
name = "openai-stt"
stt_model = "whisper-1"

[llm.stt]
provider = "quality"
"#,
        );
        // "quality" has no stt_model — returns None for name-based lookup.
        assert!(cfg.stt_provider_entry().is_none());
    }

    #[test]
    fn stt_config_deserializes_new_slim_format() {
        let cfg = parse_llm(
            r#"
[llm]

[[llm.providers]]
type = "openai"
name = "quality"
stt_model = "whisper-1"

[llm.stt]
provider = "quality"
language = "en"
"#,
        );
        let stt = cfg.stt.as_ref().expect("stt section present");
        assert_eq!(stt.provider, "quality");
        assert_eq!(stt.language, "en");
    }

    #[test]
    fn stt_config_default_provider_is_empty() {
        // Verify that W4 fix: default_stt_provider() returns "" not "whisper".
        assert_eq!(default_stt_provider(), "");
    }

    #[test]
    fn validate_stt_missing_provider_ok() {
        let cfg = parse_llm("[llm]\n");
        assert!(cfg.validate_stt().is_ok());
    }

    #[test]
    fn validate_stt_valid_reference() {
        let cfg = parse_llm(
            r#"
[llm]

[[llm.providers]]
type = "openai"
name = "quality"
stt_model = "whisper-1"

[llm.stt]
provider = "quality"
"#,
        );
        assert!(cfg.validate_stt().is_ok());
    }

    #[test]
    fn validate_stt_nonexistent_provider_errors() {
        let cfg = parse_llm(
            r#"
[llm]

[[llm.providers]]
type = "openai"
name = "quality"
model = "gpt-5.4"

[llm.stt]
provider = "nonexistent"
"#,
        );
        assert!(cfg.validate_stt().is_err());
    }

    #[test]
    fn validate_stt_provider_exists_but_no_stt_model_returns_ok_with_warn() {
        // MEDIUM: provider is found but has no stt_model — should return Ok (warn path, not error).
        let cfg = parse_llm(
            r#"
[llm]

[[llm.providers]]
type = "openai"
name = "quality"
model = "gpt-5.4"

[llm.stt]
provider = "quality"
"#,
        );
        // validate_stt must succeed (only a tracing::warn is emitted — not an error).
        assert!(cfg.validate_stt().is_ok());
        // stt_provider_entry must return None because no stt_model is set.
        assert!(
            cfg.stt_provider_entry().is_none(),
            "stt_provider_entry must be None when provider has no stt_model"
        );
    }
}
