// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt;

use serde::{Deserialize, Serialize};

// ── LLM provider config types (moved from zeph-llm) ─────────────────────────

/// Extended or adaptive thinking mode for Claude.
///
/// Serializes with `mode` as tag:
/// `{ "mode": "extended", "budget_tokens": 10000 }` or `{ "mode": "adaptive" }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ThinkingConfig {
    /// Extended thinking with an explicit token budget.
    Extended {
        /// Maximum thinking tokens to allocate.
        budget_tokens: u32,
    },
    /// Adaptive thinking that selects effort automatically.
    Adaptive {
        /// Explicit effort hint when provided; model-chosen when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<ThinkingEffort>,
    },
}

/// Effort level for adaptive thinking.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingEffort {
    /// Minimal thinking; fastest responses.
    Low,
    /// Balanced thinking depth. This is the default.
    #[default]
    Medium,
    /// Maximum thinking depth; slowest responses.
    High,
}

/// Prompt-cache TTL variant for the Anthropic API.
///
/// When used as a TOML config value the accepted strings are `"ephemeral"` and `"1h"`.
/// On the wire (Anthropic API), `OneHour` serializes as `"1h"` inside the `cache_control.ttl`
/// field.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CacheTtl {
    /// Default ephemeral TTL (~5 minutes). No beta header required.
    #[default]
    Ephemeral,
    /// Extended 1-hour TTL. Requires the `extended-cache-ttl-2025-04-25` beta header.
    /// Cache writes cost approximately 2× more than `Ephemeral`.
    #[serde(rename = "1h")]
    OneHour,
}

impl CacheTtl {
    /// Returns `true` when this TTL variant requires the `extended-cache-ttl-2025-04-25` beta
    /// header to be sent with each request.
    #[must_use]
    pub fn requires_beta(self) -> bool {
        match self {
            Self::OneHour => true,
            Self::Ephemeral => false,
        }
    }
}

/// Thinking level for Gemini models that support extended reasoning.
///
/// Maps to `generationConfig.thinkingConfig.thinkingLevel` in the Gemini API.
/// Valid for Gemini 3+ models. For Gemini 2.5, use `thinking_budget` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GeminiThinkingLevel {
    /// Minimal reasoning pass.
    Minimal,
    /// Low reasoning depth.
    Low,
    /// Medium reasoning depth.
    Medium,
    /// Full reasoning depth.
    High,
}

/// Newtype wrapper for a provider name referencing an entry in `[[llm.providers]]`.
///
/// Using a dedicated type instead of bare `String` makes provider cross-references
/// explicit in the type system and enables validation at config load time.
///
/// # Note
///
/// `zeph-common` now defines a canonical `ProviderName(Arc<str>)` newtype. This
/// config-local type uses `String` and exists for backward compat within `zeph-config`.
///
/// TODO(critic): migrate to `zeph_common::ProviderName` once `zeph-config` → `zeph-common`
/// dependency inversion (A-1) lands.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderName(String);

impl ProviderName {
    /// Create a new `ProviderName` from any string-like value.
    ///
    /// An empty string is a sentinel meaning "use the primary provider" and is the
    /// default value. Check [`is_empty`](Self::is_empty) before using in routing.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_config::providers::ProviderName;
    ///
    /// let name = ProviderName::new("fast");
    /// assert_eq!(name.as_str(), "fast");
    /// ```
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Return `true` when this is the empty sentinel (use primary provider).
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_config::providers::ProviderName;
    ///
    /// assert!(ProviderName::default().is_empty());
    /// assert!(!ProviderName::new("fast").is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Return the inner string slice.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_config::providers::ProviderName;
    ///
    /// let name = ProviderName::new("quality");
    /// assert_eq!(name.as_str(), "quality");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Return `Some(&str)` when non-empty, `None` for the empty sentinel.
    ///
    /// Bridges `Option<ProviderName>` fields and the legacy
    /// `.as_deref().filter(|s| !s.is_empty())` pattern.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_config::providers::ProviderName;
    ///
    /// assert_eq!(ProviderName::default().as_non_empty(), None);
    /// assert_eq!(ProviderName::new("fast").as_non_empty(), Some("fast"));
    /// ```
    #[must_use]
    pub fn as_non_empty(&self) -> Option<&str> {
        if self.0.is_empty() {
            None
        } else {
            Some(&self.0)
        }
    }
}

impl fmt::Display for ProviderName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for ProviderName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for ProviderName {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for ProviderName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for ProviderName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

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

/// Returns the default STT provider name (empty string — auto-detect).
#[must_use]
pub fn default_stt_provider() -> String {
    String::new()
}

/// Returns the default STT transcription language hint (`"auto"`).
#[must_use]
pub fn default_stt_language() -> String {
    "auto".into()
}

/// Returns the default embedding model name used by `[llm] embedding_model`.
#[must_use]
pub fn get_default_embedding_model() -> String {
    default_embedding_model()
}

/// Returns the default response cache TTL in seconds.
#[must_use]
pub fn get_default_response_cache_ttl_secs() -> u64 {
    default_response_cache_ttl_secs()
}

/// Returns the default EMA alpha for the router latency estimator.
#[must_use]
pub fn get_default_router_ema_alpha() -> f64 {
    default_router_ema_alpha()
}

/// Returns the default router reorder interval (turns between provider re-ranking).
#[must_use]
pub fn get_default_router_reorder_interval() -> u64 {
    default_router_reorder_interval()
}

/// LLM provider backend selector.
///
/// Used in `[[llm.providers]]` entries as the `type` field.
///
/// # Example (TOML)
///
/// ```toml
/// [[llm.providers]]
/// type = "openai"
/// model = "gpt-4o"
/// name = "quality"
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// Local Ollama server (default base URL: `http://localhost:11434`).
    Ollama,
    /// Anthropic Claude API.
    Claude,
    /// `OpenAI` API.
    OpenAi,
    /// Google Gemini API.
    Gemini,
    /// Local Candle inference (CPU/GPU, no external server required).
    Candle,
    /// OpenAI-compatible third-party API (e.g. Groq, Together AI, LM Studio).
    Compatible,
    /// Native Gonka blockchain provider.
    Gonka,
}

impl ProviderKind {
    /// Return the lowercase string identifier for this provider kind.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_config::ProviderKind;
    ///
    /// assert_eq!(ProviderKind::Claude.as_str(), "claude");
    /// assert_eq!(ProviderKind::OpenAi.as_str(), "openai");
    /// ```
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::Claude => "claude",
            Self::OpenAi => "openai",
            Self::Gemini => "gemini",
            Self::Candle => "candle",
            Self::Compatible => "compatible",
            Self::Gonka => "gonka",
        }
    }
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// LLM configuration, nested under `[llm]` in TOML.
///
/// Declares the provider pool and controls routing, embedding, caching, and STT.
/// All providers are declared in `[[llm.providers]]`; subsystems reference them by
/// the `name` field using a `*_provider` config key.
///
/// # Example (TOML)
///
/// ```toml
/// [[llm.providers]]
/// name = "fast"
/// type = "openai"
/// model = "gpt-4o-mini"
///
/// [[llm.providers]]
/// name = "quality"
/// type = "claude"
/// model = "claude-opus-4-5"
///
/// [llm]
/// routing = "none"
/// embedding_model = "qwen3-embedding"
/// ```
#[derive(Debug, Deserialize, Serialize)]
pub struct LlmConfig {
    /// Provider pool. First entry is default unless one is marked `default = true`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderEntry>,

    /// Routing strategy for multi-provider configs.
    #[serde(default, skip_serializing_if = "is_routing_none")]
    pub routing: LlmRoutingStrategy,

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

    /// Collaborative Entropy (`CoE`) configuration. `None` = `CoE` disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coe: Option<CoeConfig>,
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

    /// Effective model for the primary chat-capable provider.
    ///
    /// Skips embed-only entries (those with `embed = true`) and returns the model of the
    /// first provider that can handle chat requests. Falls back to `"qwen3:8b"` when no
    /// chat-capable provider is configured.
    #[must_use]
    pub fn effective_model(&self) -> &str {
        self.providers
            .iter()
            .find(|e| !e.embed)
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

    /// Resolve `provider_name` to its model string and emit a startup warning when the
    /// model does not look like a fast-tier model.
    ///
    /// **Soft check — never returns an error.** Misconfiguration produces a single
    /// `tracing::warn!` at startup so operators can fix configs without being blocked.
    ///
    /// Rules:
    /// - Empty `provider_name` → silently OK (caller will use the primary provider).
    /// - Provider not found in pool → warns `"<label> provider '<name>' not found"`.
    /// - Model resolved but not in `FAST_TIER_MODEL_HINTS` and not in `extra_allowlist` →
    ///   warns `"<label> provider '<name>' uses '<model>' which may not be fast-tier"`.
    /// - Model matches a hint or allowlist entry → silently OK.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_config::providers::{LlmConfig, ProviderName};
    ///
    /// // LlmConfig is constructed via config file; here we illustrate the call shape.
    /// # let cfg: LlmConfig = unimplemented!();
    /// // empty provider name is silently ok
    /// cfg.warn_non_fast_tier_provider(&ProviderName::default(), "memcot.distill_provider", &[]);
    /// ```
    pub fn warn_non_fast_tier_provider(
        &self,
        provider_name: &ProviderName,
        feature_label: &str,
        extra_allowlist: &[String],
    ) {
        if provider_name.is_empty() {
            return;
        }
        let name = provider_name.as_str();
        let Some(entry) = self.providers.iter().find(|p| p.effective_name() == name) else {
            tracing::warn!(
                provider = name,
                "{feature_label} provider '{name}' not found in [[llm.providers]]"
            );
            return;
        };
        let model = entry.model.as_deref().unwrap_or("");
        if model.is_empty() {
            return;
        }
        let lower = model.to_lowercase();
        let in_hints = FAST_TIER_MODEL_HINTS.iter().any(|h| lower.contains(h));
        let in_extra = extra_allowlist.iter().any(|h| lower.contains(h.as_str()));
        if !in_hints && !in_extra {
            tracing::warn!(
                provider = name,
                actual = model,
                "{feature_label} provider '{name}' uses model '{model}' \
                 which may not be fast-tier; prefer a fast model to bound distillation cost"
            );
        }
    }
}

/// Lowercased substrings that identify commonly accepted fast-tier models.
///
/// Used by [`LlmConfig::warn_non_fast_tier_provider`] for a soft startup check.
/// Updating this list is non-breaking; missing a fast model only suppresses a warning.
pub const FAST_TIER_MODEL_HINTS: &[&str] = &[
    "gpt-4o-mini",
    "gpt-4.1-mini",
    "gpt-5-mini",
    "gpt-5-nano",
    "claude-haiku",
    "claude-3-haiku",
    "claude-3-5-haiku",
    "qwen3:8b",
    "qwen2.5:7b",
    "qwen2:7b",
    "llama3.2:3b",
    "llama3.1:8b",
    "gemma3:4b",
    "gemma3:8b",
    "phi4:mini",
    "mistral:7b",
];

/// Speech-to-text configuration, nested under `[llm.stt]` in TOML.
///
/// When set, Zeph uses the referenced provider for voice transcription.
/// The provider must have an `stt_model` field set in its `[[llm.providers]]` entry.
///
/// # Example (TOML)
///
/// ```toml
/// [llm.stt]
/// provider = "fast"
/// language = "en"
/// ```
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
    /// PILOT: `LinUCB` contextual bandit with online learning and cost-aware reward.
    Bandit,
}

/// Agent Stability Index (ASI) configuration.
///
/// Tracks per-provider response coherence via a sliding window of response embeddings.
/// When coherence drops below `coherence_threshold`, the provider's routing prior is
/// penalized by `penalty_weight`. Disabled by default; session-only (no persistence).
///
/// # Known Limitation
///
/// ASI embeddings are computed in a background `tokio::spawn` task after the response is
/// returned to the caller. Under high request rates, the coherence score used for routing
/// may lag 1–2 responses behind due to this fire-and-forget design. With the default
/// `window = 5`, this lag is tolerable — coherence is a slow-moving signal.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AsiConfig {
    /// Enable ASI coherence tracking. Default: false.
    #[serde(default)]
    pub enabled: bool,

    /// Sliding window size for response embeddings per provider. Default: 5.
    #[serde(default = "default_asi_window")]
    pub window: usize,

    /// Coherence score [0.0, 1.0] below which the provider is penalized. Default: 0.7.
    #[serde(default = "default_asi_coherence_threshold")]
    pub coherence_threshold: f32,

    /// Penalty weight applied to Thompson beta / EMA score on low coherence. Default: 0.3.
    ///
    /// For Thompson, this shifts the beta prior: `beta += penalty_weight * (threshold - coherence)`.
    /// For EMA, the score is multiplied by `max(0.5, coherence / threshold)`.
    #[serde(default = "default_asi_penalty_weight")]
    pub penalty_weight: f32,
}

fn default_asi_window() -> usize {
    5
}

fn default_asi_coherence_threshold() -> f32 {
    0.7
}

fn default_asi_penalty_weight() -> f32 {
    0.3
}

impl Default for AsiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            window: default_asi_window(),
            coherence_threshold: default_asi_coherence_threshold(),
            penalty_weight: default_asi_penalty_weight(),
        }
    }
}

/// Routing configuration for multi-provider setups.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RouterConfig {
    /// Routing strategy: `"ema"` (default), `"thompson"`, `"cascade"`, or `"bandit"`.
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
    /// PILOT bandit routing configuration. Only used when `strategy = "bandit"`.
    #[serde(default)]
    pub bandit: Option<BanditConfig>,
    /// Embedding-based quality gate threshold for Thompson/EMA routing. Default: disabled.
    ///
    /// When set, after provider selection, the cosine similarity between the query embedding
    /// and the response embedding is computed. If below this threshold, the next provider in
    /// the ordered list is tried. On exhaustion, the best response seen is returned.
    ///
    /// Only applies to Thompson and EMA strategies. Cascade uses its own quality classifier.
    /// Fail-open: embedding errors disable the gate for that request.
    #[serde(default)]
    pub quality_gate: Option<f32>,
    /// Agent Stability Index configuration. Disabled by default.
    #[serde(default)]
    pub asi: Option<AsiConfig>,
    /// Maximum number of concurrent `embed_batch` calls through the router.
    ///
    /// Limits simultaneous embedding HTTP requests to prevent provider rate-limiting
    /// and memory pressure during indexing or high-frequency recall. Default: 4.
    /// Set to 0 to disable the semaphore (unlimited concurrency).
    #[serde(default = "default_embed_concurrency")]
    pub embed_concurrency: usize,
}

fn default_embed_concurrency() -> usize {
    4
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

fn default_bandit_alpha() -> f32 {
    1.0
}

fn default_bandit_dim() -> usize {
    32
}

fn default_bandit_cost_weight() -> f32 {
    0.1
}

fn default_bandit_decay_factor() -> f32 {
    1.0
}

fn default_bandit_embedding_timeout_ms() -> u64 {
    50
}

fn default_bandit_cache_size() -> usize {
    512
}

/// Configuration for PILOT bandit routing (`strategy = "bandit"`).
///
/// PILOT (Provider Intelligence via Learned Online Tuning) uses a `LinUCB` contextual
/// bandit to learn which provider performs best for a given query context. The feature
/// vector is derived from the query embedding (first `dim` components, L2-normalised).
///
/// **Cold start**: the bandit falls back to Thompson sampling for the first
/// `10 * num_providers` queries (configurable). After warmup, `LinUCB` takes over.
///
/// **Embedding**: an `embedding_provider` must be set for feature vectors. If the embed
/// call exceeds `embedding_timeout_ms` or fails, the bandit falls back to Thompson/uniform.
/// Use a local provider (Ollama, Candle) to avoid network latency on the hot path.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BanditConfig {
    /// `LinUCB` exploration parameter. Default: 1.0.
    /// Higher values increase exploration; lower values favour exploitation.
    #[serde(default = "default_bandit_alpha")]
    pub alpha: f32,

    /// Feature vector dimension (first `dim` components of the embedding).
    ///
    /// This is simple truncation, not PCA. The first raw embedding dimensions do not
    /// necessarily capture the most variance. For `OpenAI` `text-embedding-3-*` models,
    /// consider using the `dimensions` API parameter (Matryoshka embeddings) instead.
    /// Default: 32.
    #[serde(default = "default_bandit_dim")]
    pub dim: usize,

    /// Cost penalty weight in the reward signal: `reward = quality - cost_weight * cost_fraction`.
    /// Default: 0.1. Increase to penalise expensive providers more aggressively.
    #[serde(default = "default_bandit_cost_weight")]
    pub cost_weight: f32,

    /// Session-level decay applied to arm state on startup: `A = I + decay*(A-I)`, `b = decay*b`.
    /// Values < 1.0 cause re-exploration after provider quality changes. Default: 1.0 (no decay).
    #[serde(default = "default_bandit_decay_factor")]
    pub decay_factor: f32,

    /// Provider name from `[[llm.providers]]` used for query embeddings.
    ///
    /// SLM recommended: prefer a fast local model (e.g. Ollama `nomic-embed-text`,
    /// Candle, or `text-embedding-3-small`) — this is called on every bandit request.
    /// Empty string disables `LinUCB` (bandit always falls back to Thompson/uniform).
    #[serde(default)]
    pub embedding_provider: ProviderName,

    /// Hard timeout for the embedding call in milliseconds. Default: 50.
    /// If exceeded, the request falls back to Thompson/uniform selection.
    #[serde(default = "default_bandit_embedding_timeout_ms")]
    pub embedding_timeout_ms: u64,

    /// Maximum cached embeddings (keyed by query text hash). Default: 512.
    #[serde(default = "default_bandit_cache_size")]
    pub cache_size: usize,

    /// Path for persisting bandit state. Defaults to `~/.config/zeph/router_bandit_state.json`.
    ///
    /// # Security
    ///
    /// This path is user-controlled. The file is created with mode `0o600` on Unix.
    /// Do not place it in world-writable directories.
    #[serde(default)]
    pub state_path: Option<String>,

    /// MAR (Memory-Augmented Routing) confidence threshold.
    ///
    /// When the top-1 semantic recall score for the current query is >= this value,
    /// the bandit biases toward cheaper providers (the answer is likely in memory).
    /// Set to 1.0 to disable MAR. Default: 0.9.
    #[serde(default = "default_bandit_memory_confidence_threshold")]
    pub memory_confidence_threshold: f32,

    /// Minimum number of queries before `LinUCB` takes over from Thompson warmup.
    ///
    /// When unset or `0`, defaults to `10 × number of providers` (computed at startup).
    /// Set explicitly to control how long the bandit explores uniformly before
    /// switching to context-aware routing. Setting `0` preserves the computed default.
    #[serde(default)]
    pub warmup_queries: Option<u64>,
}

fn default_bandit_memory_confidence_threshold() -> f32 {
    0.9
}

impl Default for BanditConfig {
    fn default() -> Self {
        Self {
            alpha: default_bandit_alpha(),
            dim: default_bandit_dim(),
            cost_weight: default_bandit_cost_weight(),
            decay_factor: default_bandit_decay_factor(),
            embedding_provider: ProviderName::default(),
            embedding_timeout_ms: default_bandit_embedding_timeout_ms(),
            cache_size: default_bandit_cache_size(),
            state_path: None,
            memory_confidence_threshold: default_bandit_memory_confidence_threshold(),
            warmup_queries: None,
        }
    }
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
    /// Maximum seconds to wait for each half of a single inference request.
    ///
    /// The timeout is applied **twice** per `chat()` call: once for the channel send
    /// (waiting for a free slot) and once for the oneshot reply (waiting for the worker
    /// to finish). The effective maximum wall-clock wait per request is therefore
    /// `2 × inference_timeout_secs`. CPU inference can be slow; 120s is a conservative
    /// default for large models, giving up to 240s total before an error is returned.
    /// Values of 0 are silently promoted to 1 at bootstrap.
    #[serde(default = "default_inference_timeout_secs")]
    pub inference_timeout_secs: u64,
}

fn default_inference_timeout_secs() -> u64 {
    120
}

/// Sampling / generation parameters for Candle local inference.
///
/// Used inside `[llm.candle.generation]` or a `[[llm.providers]]` Candle entry.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GenerationParams {
    /// Sampling temperature. Higher values produce more creative outputs. Default: `0.7`.
    #[serde(default = "default_temperature")]
    pub temperature: f64,
    /// Nucleus sampling threshold. When set, tokens with cumulative probability above
    /// this value are excluded. Default: `None` (disabled).
    #[serde(default)]
    pub top_p: Option<f64>,
    /// Top-k sampling. When set, only the top-k most probable tokens are considered.
    /// Default: `None` (disabled).
    #[serde(default)]
    pub top_k: Option<usize>,
    /// Maximum number of tokens to generate per response. Capped at [`MAX_TOKENS_CAP`].
    /// Default: `2048`.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    /// Random seed for reproducible outputs. Default: `42`.
    #[serde(default = "default_seed")]
    pub seed: u64,
    /// Repetition penalty applied during sampling. Default: `1.1`.
    #[serde(default = "default_repeat_penalty")]
    pub repeat_penalty: f32,
    /// Number of last tokens to consider for the repetition penalty window. Default: `64`.
    #[serde(default = "default_repeat_last_n")]
    pub repeat_last_n: usize,
}

/// Hard upper bound on `GenerationParams::max_tokens` to prevent unbounded generation.
pub const MAX_TOKENS_CAP: usize = 32768;

impl GenerationParams {
    /// Returns `max_tokens` clamped to [`MAX_TOKENS_CAP`].
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_config::GenerationParams;
    ///
    /// let params = GenerationParams::default();
    /// assert!(params.capped_max_tokens() <= 32768);
    /// ```
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
    /// Complexity triage routing: pre-classify each request, delegate to appropriate tier.
    Triage,
    /// PILOT: `LinUCB` contextual bandit with online learning and budget-aware reward.
    Bandit,
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
    pub triage_provider: Option<ProviderName>,

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

/// Configuration for the Collaborative Entropy (`CoE`) subsystem (`[llm.coe]` TOML section).
///
/// `CoE` detects uncertain responses from the primary provider and escalates to a
/// secondary provider when either the intra-entropy or inter-divergence signal crosses
/// its threshold. Only active for `RouterStrategy::Ema` and `RouterStrategy::Thompson`.
///
/// # Example
///
/// ```toml
/// [llm.coe]
/// enabled = true
/// intra_threshold = 0.8
/// inter_threshold = 0.20
/// shadow_sample_rate = 0.1
/// secondary_provider = "quality"
/// embed_provider = ""
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CoeConfig {
    /// Enable `CoE`. When `false`, the struct is ignored.
    pub enabled: bool,
    /// Mean negative log-prob threshold; responses above this trigger intra escalation.
    pub intra_threshold: f64,
    /// Divergence threshold in `[0.0, 1.0]`.
    pub inter_threshold: f64,
    /// Baseline rate at which secondary is called even when intra is low.
    pub shadow_sample_rate: f64,
    /// Provider name from `[[llm.providers]]` used as the escalation target.
    pub secondary_provider: ProviderName,
    /// Provider name for inter-divergence embeddings. Empty → inherit bandit's embed provider.
    pub embed_provider: ProviderName,
}

impl Default for CoeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            intra_threshold: 0.8,
            inter_threshold: 0.20,
            shadow_sample_rate: 0.1,
            secondary_provider: ProviderName::default(),
            embed_provider: ProviderName::default(),
        }
    }
}

/// A single Gonka network node endpoint.
///
/// Used in `[[llm.providers]]` entries with `type = "gonka"` to declare
/// the node pool for blockchain inference routing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct GonkaNode {
    /// HTTP(S) URL of the Gonka node (e.g. `"https://node1.gonka.ai"`).
    pub url: String,
    /// On-chain bech32 address of this node (e.g. `"gonka1w508d6qejxtdg4y5r3zarvary0c5xw7k2gsyg6"`).
    ///
    /// Required for signature construction: every signed request binds to the target node's
    /// on-chain address, making signatures non-replayable across different nodes.
    pub address: String,
    /// Optional human-readable label for `zeph gonka doctor` output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
    /// Maximum wall-clock seconds to wait for a single inference request.
    ///
    /// Effective timeout is `2 × inference_timeout_secs` (send + recv each have this budget).
    /// CPU inference can be slow; 120s is a conservative default. Floored at 1s.
    #[serde(default = "default_inference_timeout_secs")]
    pub inference_timeout_secs: u64,
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
            inference_timeout_secs: default_inference_timeout_secs(),
        }
    }
}

/// Unified provider entry: one struct replaces `CloudLlmConfig`, `OpenAiConfig`,
/// `GeminiConfig`, `OllamaConfig`, `CompatibleConfig`, and `OrchestratorProviderConfig`.
///
/// Provider-specific fields use `#[serde(default)]` and are ignored by backends
/// that do not use them (flat-union pattern).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)] // config struct — boolean flags are idiomatic for TOML-deserialized configuration
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
    /// Prompt cache TTL variant. `None` keeps the default ~5-minute ephemeral TTL.
    /// Set to `"1h"` to enable the extended 1-hour TTL (beta, ~2× write cost).
    #[serde(default)]
    pub prompt_cache_ttl: Option<CacheTtl>,

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

    // --- Compatible-specific: optional inline api_key ---
    #[serde(default)]
    pub api_key: Option<String>,

    // --- Candle-specific ---
    #[serde(default)]
    pub candle: Option<CandleInlineConfig>,

    // --- Vision ---
    #[serde(default)]
    pub vision_model: Option<String>,

    // --- Gonka-specific ---
    /// Gonka network node pool. Required (non-empty) when `type = "gonka"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gonka_nodes: Vec<GonkaNode>,
    /// bech32 chain prefix for address encoding. Defaults to `"gonka"` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gonka_chain_prefix: Option<String>,

    /// Provider-specific instruction file.
    #[serde(default)]
    pub instruction_file: Option<std::path::PathBuf>,

    /// Maximum concurrent LLM calls from orchestrated sub-agents to this provider.
    ///
    /// When set, `DagScheduler` acquires a semaphore permit before dispatching a
    /// sub-agent that targets this provider. Dispatch is deferred (using the existing
    /// `deferral_backoff` mechanism) when the semaphore is saturated.
    ///
    /// `None` (default) = unlimited — no admission control applied.
    ///
    /// # Example (TOML)
    ///
    /// ```toml
    /// [[llm.providers]]
    /// name = "quality"
    /// type = "openai"
    /// model = "gpt-5"
    /// max_concurrent = 3
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,
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
            prompt_cache_ttl: None,
            reasoning_effort: None,
            thinking_level: None,
            thinking_budget: None,
            include_thoughts: None,
            api_key: None,
            candle: None,
            vision_model: None,
            gonka_nodes: Vec::new(),
            gonka_chain_prefix: None,
            instruction_file: None,
            max_concurrent: None,
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
            // Compatible/Candle return empty because the model is resolved elsewhere.
            // Gonka returns empty because it is a blockchain provider, not an LLM — there is no model concept.
            ProviderKind::Compatible | ProviderKind::Candle | ProviderKind::Gonka => String::new(),
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

        // B3: gonka provider MUST have name and valid gonka_nodes.
        if self.provider_type == ProviderKind::Gonka {
            if self.name.is_none() {
                return Err(ConfigError::Validation(
                    "[[llm.providers]] entry with type=\"gonka\" must set `name`".into(),
                ));
            }
            self.validate_gonka_nodes()?;
        }

        // B1: warn on irrelevant fields.
        self.warn_irrelevant_fields();

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

    /// Resolve the effective Gonka chain prefix: explicit value or `"gonka"` default.
    #[must_use]
    pub fn effective_gonka_chain_prefix(&self) -> &str {
        self.gonka_chain_prefix.as_deref().unwrap_or("gonka")
    }

    fn warn_irrelevant_fields(&self) {
        let name = self.effective_name();
        match self.provider_type {
            ProviderKind::Ollama => {
                if self.thinking.is_some() {
                    tracing::warn!(
                        provider = name,
                        "field `thinking` is only used by Claude providers"
                    );
                }
                if self.reasoning_effort.is_some() {
                    tracing::warn!(
                        provider = name,
                        "field `reasoning_effort` is only used by OpenAI providers"
                    );
                }
                if self.thinking_level.is_some() || self.thinking_budget.is_some() {
                    tracing::warn!(
                        provider = name,
                        "fields `thinking_level`/`thinking_budget` are only used by Gemini providers"
                    );
                }
            }
            ProviderKind::Claude => {
                if self.reasoning_effort.is_some() {
                    tracing::warn!(
                        provider = name,
                        "field `reasoning_effort` is only used by OpenAI providers"
                    );
                }
                if self.thinking_level.is_some() || self.thinking_budget.is_some() {
                    tracing::warn!(
                        provider = name,
                        "fields `thinking_level`/`thinking_budget` are only used by Gemini providers"
                    );
                }
            }
            ProviderKind::OpenAi => {
                if self.thinking.is_some() {
                    tracing::warn!(
                        provider = name,
                        "field `thinking` is only used by Claude providers"
                    );
                }
                if self.thinking_level.is_some() || self.thinking_budget.is_some() {
                    tracing::warn!(
                        provider = name,
                        "fields `thinking_level`/`thinking_budget` are only used by Gemini providers"
                    );
                }
            }
            ProviderKind::Gemini => {
                if self.thinking.is_some() {
                    tracing::warn!(
                        provider = name,
                        "field `thinking` is only used by Claude providers"
                    );
                }
                if self.reasoning_effort.is_some() {
                    tracing::warn!(
                        provider = name,
                        "field `reasoning_effort` is only used by OpenAI providers"
                    );
                }
            }
            ProviderKind::Gonka => {
                if self.thinking.is_some() {
                    tracing::warn!(
                        provider = name,
                        "field `thinking` is only used by Claude providers"
                    );
                }
                if self.reasoning_effort.is_some() {
                    tracing::warn!(
                        provider = name,
                        "field `reasoning_effort` is only used by OpenAI providers"
                    );
                }
                if self.thinking_level.is_some() || self.thinking_budget.is_some() {
                    tracing::warn!(
                        provider = name,
                        "fields `thinking_level`/`thinking_budget` are only used by Gemini providers"
                    );
                }
            }
            ProviderKind::Compatible | ProviderKind::Candle => {}
        }
    }

    fn validate_gonka_nodes(&self) -> Result<(), crate::error::ConfigError> {
        use crate::error::ConfigError;
        if self.gonka_nodes.is_empty() {
            return Err(ConfigError::Validation(format!(
                "[[llm.providers]] entry '{}' with type=\"gonka\" must set non-empty `gonka_nodes`",
                self.effective_name()
            )));
        }
        for (i, node) in self.gonka_nodes.iter().enumerate() {
            if node.url.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "[[llm.providers]] entry '{}' gonka_nodes[{i}].url must not be empty",
                    self.effective_name()
                )));
            }
            if !node.url.starts_with("http://") && !node.url.starts_with("https://") {
                return Err(ConfigError::Validation(format!(
                    "[[llm.providers]] entry '{}' gonka_nodes[{i}].url must start with http:// or https://",
                    self.effective_name()
                )));
            }
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
    fn effective_model_skips_embed_only_provider() {
        let cfg = parse_llm(
            r#"
[llm]

[[llm.providers]]
type = "ollama"
model = "gemma4:26b"
embed = true

[[llm.providers]]
type = "openai"
model = "gpt-4o-mini"
"#,
        );
        assert_eq!(cfg.effective_model(), "gpt-4o-mini");
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

    // ─── BanditConfig::warmup_queries deserialization ─────────────────────────

    #[test]
    fn bandit_warmup_queries_explicit_value_is_deserialized() {
        let cfg = parse_llm(
            r#"
[llm]

[llm.router]
strategy = "bandit"

[llm.router.bandit]
warmup_queries = 50
"#,
        );
        let bandit = cfg
            .router
            .expect("router section must be present")
            .bandit
            .expect("bandit section must be present");
        assert_eq!(
            bandit.warmup_queries,
            Some(50),
            "warmup_queries = 50 must deserialize to Some(50)"
        );
    }

    #[test]
    fn bandit_warmup_queries_explicit_null_is_none() {
        // Explicitly writing the field as absent: field simply not present is
        // equivalent due to #[serde(default)]. Test that an explicit 0 is Some(0).
        let cfg = parse_llm(
            r#"
[llm]

[llm.router]
strategy = "bandit"

[llm.router.bandit]
warmup_queries = 0
"#,
        );
        let bandit = cfg
            .router
            .expect("router section must be present")
            .bandit
            .expect("bandit section must be present");
        // 0 is a valid explicit value — it means "preserve computed default".
        assert_eq!(
            bandit.warmup_queries,
            Some(0),
            "warmup_queries = 0 must deserialize to Some(0)"
        );
    }

    #[test]
    fn bandit_warmup_queries_missing_field_defaults_to_none() {
        // When warmup_queries is omitted entirely, #[serde(default)] must produce None.
        let cfg = parse_llm(
            r#"
[llm]

[llm.router]
strategy = "bandit"

[llm.router.bandit]
alpha = 1.5
"#,
        );
        let bandit = cfg
            .router
            .expect("router section must be present")
            .bandit
            .expect("bandit section must be present");
        assert_eq!(
            bandit.warmup_queries, None,
            "omitted warmup_queries must default to None"
        );
    }

    #[test]
    fn provider_name_new_and_as_str() {
        let n = ProviderName::new("fast");
        assert_eq!(n.as_str(), "fast");
        assert!(!n.is_empty());
    }

    #[test]
    fn provider_name_default_is_empty() {
        let n = ProviderName::default();
        assert!(n.is_empty());
        assert_eq!(n.as_str(), "");
    }

    #[test]
    fn provider_name_deref_to_str() {
        let n = ProviderName::new("quality");
        let s: &str = &n;
        assert_eq!(s, "quality");
    }

    #[test]
    fn provider_name_partial_eq_str() {
        let n = ProviderName::new("fast");
        assert_eq!(n, "fast");
        assert_ne!(n, "slow");
    }

    #[test]
    fn provider_name_serde_roundtrip() {
        let n = ProviderName::new("my-provider");
        let json = serde_json::to_string(&n).expect("serialize");
        assert_eq!(json, "\"my-provider\"");
        let back: ProviderName = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, n);
    }

    #[test]
    fn provider_name_serde_empty_roundtrip() {
        let n = ProviderName::default();
        let json = serde_json::to_string(&n).expect("serialize");
        assert_eq!(json, "\"\"");
        let back: ProviderName = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, n);
        assert!(back.is_empty());
    }

    // ─── GonkaNode / ProviderKind::Gonka ─────────────────────────────────────

    fn gonka_entry_with_nodes(nodes: Vec<GonkaNode>) -> ProviderEntry {
        ProviderEntry {
            provider_type: ProviderKind::Gonka,
            name: Some("my-gonka".into()),
            gonka_nodes: nodes,
            ..Default::default()
        }
    }

    fn valid_gonka_nodes() -> Vec<GonkaNode> {
        vec![
            GonkaNode {
                url: "https://node1.gonka.ai".into(),
                address: "gonka1w508d6qejxtdg4y5r3zarvary0c5xw7k2gsyg6".into(),
                name: Some("node1".into()),
            },
            GonkaNode {
                url: "https://node2.gonka.ai".into(),
                address: "gonka14h0ycu78h88wzldxc7e79vhw5xsde0n85evmum".into(),
                name: Some("node2".into()),
            },
            GonkaNode {
                url: "http://node3.internal".into(),
                address: "gonka1qyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqg".into(),
                name: None,
            },
        ]
    }

    #[test]
    fn validate_gonka_valid() {
        let entry = gonka_entry_with_nodes(valid_gonka_nodes());
        assert!(entry.validate().is_ok());
    }

    #[test]
    fn validate_gonka_empty_nodes_errors() {
        let entry = gonka_entry_with_nodes(vec![]);
        let err = entry.validate().unwrap_err();
        assert!(
            err.to_string().contains("gonka_nodes"),
            "error should mention gonka_nodes: {err}"
        );
    }

    #[test]
    fn validate_gonka_node_empty_url_errors() {
        let entry = gonka_entry_with_nodes(vec![GonkaNode {
            url: String::new(),
            address: "gonka1test".into(),
            name: None,
        }]);
        let err = entry.validate().unwrap_err();
        assert!(err.to_string().contains("url"), "{err}");
    }

    #[test]
    fn validate_gonka_node_invalid_scheme_errors() {
        let entry = gonka_entry_with_nodes(vec![GonkaNode {
            url: "ftp://node.gonka.ai".into(),
            address: "gonka1test".into(),
            name: None,
        }]);
        let err = entry.validate().unwrap_err();
        assert!(err.to_string().contains("http"), "{err}");
    }

    #[test]
    fn validate_gonka_without_name_errors() {
        let entry = ProviderEntry {
            provider_type: ProviderKind::Gonka,
            name: None,
            gonka_nodes: valid_gonka_nodes(),
            ..Default::default()
        };
        let err = entry.validate().unwrap_err();
        assert!(err.to_string().contains("gonka"), "{err}");
    }

    #[test]
    fn gonka_toml_round_trip() {
        let toml = r#"
[llm]

[[llm.providers]]
type = "gonka"
name = "my-gonka"
gonka_chain_prefix = "custom-chain"

[[llm.providers.gonka_nodes]]
url = "https://node1.gonka.ai"
address = "gonka1w508d6qejxtdg4y5r3zarvary0c5xw7k2gsyg6"
name = "node1"

[[llm.providers.gonka_nodes]]
url = "https://node2.gonka.ai"
address = "gonka14h0ycu78h88wzldxc7e79vhw5xsde0n85evmum"
name = "node2"

[[llm.providers.gonka_nodes]]
url = "https://node3.gonka.ai"
address = "gonka1qyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqg"
"#;
        let cfg = parse_llm(toml);
        assert_eq!(cfg.providers.len(), 1);
        let entry = &cfg.providers[0];
        assert_eq!(entry.provider_type, ProviderKind::Gonka);
        assert_eq!(entry.name.as_deref(), Some("my-gonka"));
        let nodes = &entry.gonka_nodes;
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[0].url, "https://node1.gonka.ai");
        assert_eq!(
            nodes[0].address,
            "gonka1w508d6qejxtdg4y5r3zarvary0c5xw7k2gsyg6"
        );
        assert_eq!(nodes[0].name.as_deref(), Some("node1"));
        assert_eq!(nodes[2].name, None);
        assert_eq!(entry.gonka_chain_prefix.as_deref(), Some("custom-chain"));
    }

    #[test]
    fn gonka_default_chain_prefix() {
        let entry = gonka_entry_with_nodes(valid_gonka_nodes());
        assert_eq!(entry.effective_gonka_chain_prefix(), "gonka");
    }

    #[test]
    fn gonka_explicit_chain_prefix() {
        let entry = ProviderEntry {
            provider_type: ProviderKind::Gonka,
            name: Some("my-gonka".into()),
            gonka_nodes: valid_gonka_nodes(),
            gonka_chain_prefix: Some("my-chain".into()),
            ..Default::default()
        };
        assert_eq!(entry.effective_gonka_chain_prefix(), "my-chain");
    }

    #[test]
    fn effective_model_gonka_is_empty() {
        let entry = ProviderEntry {
            provider_type: ProviderKind::Gonka,
            model: None,
            ..Default::default()
        };
        assert_eq!(entry.effective_model(), "");
    }

    #[test]
    fn existing_configs_still_parse() {
        let toml = r#"
[llm]

[[llm.providers]]
type = "ollama"
model = "qwen3:8b"

[[llm.providers]]
type = "claude"
name = "claude"
model = "claude-sonnet-4-6"
"#;
        let cfg = parse_llm(toml);
        assert_eq!(cfg.providers.len(), 2);
        assert_eq!(cfg.providers[0].provider_type, ProviderKind::Ollama);
        assert_eq!(cfg.providers[1].provider_type, ProviderKind::Claude);
    }
}
