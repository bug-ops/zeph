// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Multi-provider routing strategy configuration.
//!
//! Declares the routing strategy selectors ([`LlmRoutingStrategy`],
//! [`RouterStrategyConfig`]) and the per-strategy tuning structs: EMA/Thompson
//! ([`RouterConfig`]), cascade ([`CascadeConfig`]), bandit ([`BanditConfig`]),
//! reputation ([`ReputationConfig`]), stability index ([`AsiConfig`]), complexity
//! triage ([`ComplexityRoutingConfig`]), and collaborative entropy ([`CoeConfig`]).

use serde::{Deserialize, Serialize};
use zeph_common::ProviderName;

use super::default_true;

fn default_cascade_quality_threshold() -> f64 {
    0.5
}

fn default_cascade_max_escalations() -> u8 {
    2
}

fn default_cascade_window_size() -> usize {
    50
}

fn default_cascade_judge_timeout_ms() -> u64 {
    5_000
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
/// Routing strategy selection for multi-provider routing.
#[non_exhaustive]
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

    /// Hard timeout for the judge LLM call (milliseconds).
    /// If the judge does not respond within this budget, the call is treated as a failure
    /// and heuristic scoring is used instead. Default: 5000 (5 s).
    #[serde(default = "default_cascade_judge_timeout_ms")]
    pub judge_timeout_ms: u64,
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
            judge_timeout_ms: default_cascade_judge_timeout_ms(),
        }
    }
}

/// Quality classifier mode for cascade routing.
#[non_exhaustive]
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
/// Routing strategy for the `[[llm.providers]]` pool.
#[non_exhaustive]
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
/// embedding_provider = ""
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
    /// Provider name for inter-divergence embeddings. Empty → inherit bandit's embedding provider.
    pub embedding_provider: ProviderName,
}

impl Default for CoeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            intra_threshold: 0.8,
            inter_threshold: 0.20,
            shadow_sample_rate: 0.1,
            secondary_provider: ProviderName::default(),
            embedding_provider: ProviderName::default(),
        }
    }
}
