// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Core LLM configuration: the `[llm]` section, provider-kind selector, streaming
//! limits, and speech-to-text config.
//!
//! [`LlmConfig`] is the root of the LLM subsystem config; it owns the provider pool
//! ([`super::ProviderEntry`]) and references the routing, candle, and STT config types.

use serde::{Deserialize, Serialize};
use zeph_common::ProviderName;

use super::{
    CandleConfig, CoeConfig, ComplexityRoutingConfig, LlmRoutingStrategy, ProviderEntry,
    RouterConfig,
};

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
/// Returns the default STT transcription language hint (`"auto"`).
#[must_use]
pub fn default_stt_language() -> String {
    "auto".into()
}

/// Returns the default embedding model name used by `[llm] embedding_model`.
#[must_use]
pub(crate) fn get_default_embedding_model() -> String {
    default_embedding_model()
}

/// Returns the default response cache TTL in seconds.
#[must_use]
pub(crate) fn get_default_response_cache_ttl_secs() -> u64 {
    default_response_cache_ttl_secs()
}

/// Returns the default EMA alpha for the router latency estimator.
#[must_use]
pub(crate) fn get_default_router_ema_alpha() -> f64 {
    default_router_ema_alpha()
}

/// Returns the default router reorder interval (turns between provider re-ranking).
#[must_use]
pub(crate) fn get_default_router_reorder_interval() -> u64 {
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
#[non_exhaustive]
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
    /// Cocoon confidential compute network via localhost sidecar.
    Cocoon,
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
            Self::Cocoon => "cocoon",
        }
    }
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.as_str())
    }
}

fn default_max_tool_json_bytes() -> usize {
    4 * 1024 * 1024
}

fn default_max_thinking_bytes() -> usize {
    1024 * 1024
}

fn default_max_compaction_bytes() -> usize {
    32 * 1024
}

fn stream_limits_is_default(v: &StreamLimits) -> bool {
    v.max_tool_json_bytes == default_max_tool_json_bytes()
        && v.max_thinking_bytes == default_max_thinking_bytes()
        && v.max_compaction_bytes == default_max_compaction_bytes()
}

/// Per-buffer byte caps for Claude SSE streaming.
///
/// Controls the maximum number of bytes accumulated in each streaming buffer before
/// excess data is discarded with a warning. All caps default to values that match the
/// pre-existing hardcoded constants, so omitting `[llm.stream_limits]` in the config
/// preserves identical behavior.
///
/// # Example (TOML)
///
/// ```toml
/// [llm.stream_limits]
/// max_tool_json_bytes  = 8388608   # 8 MiB  — raise for unusually large tool results
/// max_thinking_bytes   = 2097152   # 2 MiB  — raise for deep extended-thinking runs
/// max_compaction_bytes = 65536     # 64 KiB — raise for verbose compaction summaries
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct StreamLimits {
    /// Maximum bytes for an accumulated tool-use JSON buffer. Default: 4 MiB.
    #[serde(default = "default_max_tool_json_bytes")]
    pub max_tool_json_bytes: usize,

    /// Maximum bytes for an accumulated thinking block. Default: 1 MiB.
    #[serde(default = "default_max_thinking_bytes")]
    pub max_thinking_bytes: usize,

    /// Maximum bytes for an accumulated server-side compaction summary. Default: 32 KiB.
    #[serde(default = "default_max_compaction_bytes")]
    pub max_compaction_bytes: usize,
}

impl Default for StreamLimits {
    fn default() -> Self {
        Self {
            max_tool_json_bytes: default_max_tool_json_bytes(),
            max_thinking_bytes: default_max_thinking_bytes(),
            max_compaction_bytes: default_max_compaction_bytes(),
        }
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

    /// SSE streaming buffer size limits.
    ///
    /// Controls the maximum bytes accumulated in per-block SSE buffers before excess
    /// data is silently discarded. All fields have sane defaults; omitting the section
    /// keeps pre-existing behavior.
    #[serde(default, skip_serializing_if = "stream_limits_is_default")]
    pub stream_limits: StreamLimits,
}

fn default_embedding_model_opt() -> String {
    default_embedding_model()
}

impl Default for LlmConfig {
    fn default() -> Self {
        toml::from_str("").expect("empty TOML produces valid LlmConfig defaults")
    }
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

    /// Returns the name of the effective embedding model.
    ///
    /// Resolution order:
    /// 1. `embedding_model` from the `[[llm.providers]]` entry marked `embed = true`
    /// 2. `embedding_model` from the first entry in `[[llm.providers]]`
    /// 3. `[llm] embedding_model` global fallback (defaults to `"nomic-embed-text"`)
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_config::providers::LlmConfig;
    ///
    /// let cfg = LlmConfig::default();
    /// assert!(!cfg.effective_embedding_model().is_empty());
    /// ```
    #[must_use]
    pub fn effective_embedding_model(&self) -> String {
        if let Some(m) = self
            .providers
            .iter()
            .find(|e| e.embed)
            .and_then(|e| e.embedding_model.as_ref())
        {
            return m.clone();
        }
        if let Some(m) = self
            .providers
            .first()
            .and_then(|e| e.embedding_model.as_ref())
        {
            return m.clone();
        }
        self.embedding_model.clone()
    }

    /// Returns the name of the stable skill embedding model.
    ///
    /// Prefers the `[[llm.providers]]` entry with `embed = true`, using its
    /// `embedding_model` field first and `model` field as a secondary fallback.
    /// Falls back to [`Self::effective_embedding_model`] when no dedicated embed
    /// entry exists. Using the actual provider model name prevents false-positive
    /// collection rebuilds in `zeph_memory::embedding_registry`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_config::providers::LlmConfig;
    ///
    /// let cfg = LlmConfig::default();
    /// assert!(!cfg.stable_skill_embedding_model().is_empty());
    /// ```
    #[must_use]
    pub fn stable_skill_embedding_model(&self) -> String {
        let embed_entry = self
            .providers
            .iter()
            .find(|e| e.embed)
            .or_else(|| self.providers.iter().find(|e| e.embedding_model.is_some()));

        if let Some(entry) = embed_entry {
            if let Some(em) = entry.embedding_model.as_ref().filter(|s| !s.is_empty()) {
                return em.clone();
            }
            if let Some(m) = entry.model.as_ref().filter(|s| !s.is_empty()) {
                return m.clone();
            }
        }

        self.effective_embedding_model()
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
    #[must_use = "validation result must be checked"]
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
            .find(|p| p.effective_name() == stt.provider.as_str());
        match found {
            None => {
                return Err(ConfigError::Validation(format!(
                    "[llm.stt].provider = {:?} does not match any [[llm.providers]] entry",
                    stt.provider.as_str()
                )));
            }
            Some(entry) if entry.stt_model.is_none() => {
                tracing::warn!(
                    provider = stt.provider.as_str(),
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
    /// Provider name from `[[llm.providers]]`. Empty means auto-detect the first provider
    /// with `stt_model` set.
    #[serde(default)]
    pub provider: ProviderName,
    /// Language hint for transcription (e.g. `"en"`, `"auto"`).
    #[serde(default = "default_stt_language")]
    pub language: String,
}
