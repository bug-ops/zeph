// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Retrieval, ranking, and admission-control configuration.
//!
//! Hybrid vector/keyword retrieval, `MemFlow` tiered retrieval, A-MAC admission
//! control, store routing, consolidation sweeps, and retrieval-failure capture.

use crate::providers::ProviderName;
use serde::{Deserialize, Serialize};
use zeph_common::memory::MemoryRoute;

use super::default_embed_timeout_secs;

fn validate_tier_similarity_threshold<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <f32 as serde::Deserialize>::deserialize(deserializer)?;
    if value.is_nan() || value.is_infinite() {
        return Err(serde::de::Error::custom(
            "similarity_threshold must be a finite number",
        ));
    }
    if !(0.5..=1.0).contains(&value) {
        return Err(serde::de::Error::custom(
            "similarity_threshold must be in [0.5, 1.0]",
        ));
    }
    Ok(value)
}

fn validate_tier_promotion_min_sessions<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <u32 as serde::Deserialize>::deserialize(deserializer)?;
    if value < 2 {
        return Err(serde::de::Error::custom(
            "promotion_min_sessions must be >= 2",
        ));
    }
    Ok(value)
}

fn validate_tier_sweep_batch_size<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <usize as serde::Deserialize>::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom("sweep_batch_size must be >= 1"));
    }
    Ok(value)
}

fn default_tier_promotion_min_sessions() -> u32 {
    3
}

fn default_tier_similarity_threshold() -> f32 {
    0.92
}

fn default_tier_sweep_interval_secs() -> u64 {
    3600
}

fn default_tier_sweep_batch_size() -> usize {
    100
}

fn default_scene_similarity_threshold() -> f32 {
    0.80
}

fn default_scene_batch_size() -> usize {
    50
}

fn validate_scene_similarity_threshold<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <f32 as serde::Deserialize>::deserialize(deserializer)?;
    if value.is_nan() || value.is_infinite() {
        return Err(serde::de::Error::custom(
            "scene_similarity_threshold must be a finite number",
        ));
    }
    if !(0.5..=1.0).contains(&value) {
        return Err(serde::de::Error::custom(
            "scene_similarity_threshold must be in [0.5, 1.0]",
        ));
    }
    Ok(value)
}

fn validate_scene_batch_size<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <usize as serde::Deserialize>::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom("scene_batch_size must be >= 1"));
    }
    Ok(value)
}

/// Configuration for the AOI three-layer memory tier promotion system (`[memory.tiers]`).
///
/// When `enabled = true`, a background sweep promotes frequently-accessed episodic messages
/// to semantic tier by clustering near-duplicates and distilling them via an LLM call.
///
/// # Validation
///
/// Constraints enforced at deserialization time:
/// - `similarity_threshold` in `[0.5, 1.0]`
/// - `promotion_min_sessions >= 2`
/// - `sweep_batch_size >= 1`
/// - `scene_similarity_threshold` in `[0.5, 1.0]`
/// - `scene_batch_size >= 1`
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct TierConfig {
    /// Enable the tier promotion system. When `false`, all messages remain episodic.
    /// Default: `false`.
    pub enabled: bool,
    /// Minimum number of distinct sessions a fact must appear in before promotion.
    /// Must be `>= 2`. Default: `3`.
    #[serde(deserialize_with = "validate_tier_promotion_min_sessions")]
    pub promotion_min_sessions: u32,
    /// Cosine similarity threshold for clustering near-duplicate facts during sweep.
    /// Must be in `[0.5, 1.0]`. Default: `0.92`.
    #[serde(deserialize_with = "validate_tier_similarity_threshold")]
    pub similarity_threshold: f32,
    /// How often the background promotion sweep runs, in seconds. Default: `3600`.
    pub sweep_interval_secs: u64,
    /// Maximum number of messages to evaluate per sweep cycle. Must be `>= 1`. Default: `100`.
    #[serde(deserialize_with = "validate_tier_sweep_batch_size")]
    pub sweep_batch_size: usize,
    /// Enable `MemScene` consolidation of semantic-tier messages. Default: `false`.
    pub scene_enabled: bool,
    /// Cosine similarity threshold for `MemScene` clustering. Must be in `[0.5, 1.0]`. Default: `0.80`.
    #[serde(deserialize_with = "validate_scene_similarity_threshold")]
    pub scene_similarity_threshold: f32,
    /// Maximum unassigned semantic messages processed per scene consolidation sweep. Default: `50`.
    #[serde(deserialize_with = "validate_scene_batch_size")]
    pub scene_batch_size: usize,
    /// Provider name from `[[llm.providers]]` for scene label/profile generation.
    /// Falls back to the primary provider when empty. Default: `""`.
    pub scene_provider: ProviderName,
    /// How often the background scene consolidation sweep runs, in seconds. Default: `7200`.
    pub scene_sweep_interval_secs: u64,
}

fn default_scene_sweep_interval_secs() -> u64 {
    7200
}

impl Default for TierConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            promotion_min_sessions: default_tier_promotion_min_sessions(),
            similarity_threshold: default_tier_similarity_threshold(),
            sweep_interval_secs: default_tier_sweep_interval_secs(),
            sweep_batch_size: default_tier_sweep_batch_size(),
            scene_enabled: false,
            scene_similarity_threshold: default_scene_similarity_threshold(),
            scene_batch_size: default_scene_batch_size(),
            scene_provider: ProviderName::default(),
            scene_sweep_interval_secs: default_scene_sweep_interval_secs(),
        }
    }
}

// ── MemFlow tiered retrieval config (issue #3712) ──────────────────────────────

/// `MemFlow` tiered intent-driven retrieval configuration.
///
/// Classifies each recall query into one of three intent tiers (`ProfileLookup`,
/// `TargetedRetrieval`, `DeepReasoning`) and dispatches to the cheapest sufficient backend.
/// An optional validation step can escalate to a heavier tier when evidence confidence is low.
///
/// # Example (TOML)
///
/// ```toml
/// [memory.tiered_retrieval]
/// enabled = false
/// classifier_provider = ""
/// validator_provider = ""
/// token_budget = 4096
/// validation_enabled = false
/// validation_threshold = 0.6
/// max_escalations = 1
/// classifier_timeout_secs = 5
/// validator_timeout_secs = 5
///
/// # Signal weights (all default to 0.0; set to activate each signal)
/// similarity_weight = 1.0
/// recency_weight = 0.0
/// recency_half_life_days = 7
/// tfidf_weight = 0.0
/// cognitive_signal_weight = 0.0
/// tier_boost_weight = 0.0
/// semantic_tier_boost = 1.0
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TieredRetrievalConfig {
    /// Enable `MemFlow` tiered retrieval. Default: `false`.
    pub enabled: bool,
    /// Provider name from `[[llm.providers]]` for intent classification.
    ///
    /// When empty, the `HeuristicRouter` is used (no LLM call). When a provider
    /// is set but the call fails, falls back to the heuristic (fail-open).
    pub classifier_provider: ProviderName,
    /// Provider name from `[[llm.providers]]` for evidence validation.
    ///
    /// When empty or when `validation_enabled = false`, no validation call is made.
    pub validator_provider: ProviderName,
    /// Maximum tokens to gather for evidence per query. Default: `4096`.
    pub token_budget: usize,
    /// Enable evidence validation and tier escalation. Default: `false`.
    pub validation_enabled: bool,
    /// Confidence threshold below which validation triggers tier escalation. Default: `0.6`.
    pub validation_threshold: f32,
    /// Maximum tier escalations per query. Default: `1`.
    pub max_escalations: u8,
    /// Timeout in seconds for the classifier LLM call. Default: `5`.
    ///
    /// On timeout the pipeline falls back to the `HeuristicRouter` (fail-open).
    pub classifier_timeout_secs: u64,
    /// Timeout in seconds for the validator LLM call. Default: `5`.
    ///
    /// On timeout the validator is treated as sufficient (fail-open).
    pub validator_timeout_secs: u64,

    // ── Signal weights ────────────────────────────────────────────────────────
    /// Weight applied to the raw similarity score from vector/keyword recall. Default: `1.0`.
    ///
    /// Set to `1.0` and all other weights to `0.0` to reproduce pre-signal behaviour.
    pub similarity_weight: f64,
    /// Weight applied to the recency decay signal. Default: `0.0` (disabled).
    pub recency_weight: f64,
    /// Half-life for recency decay in days. Default: `7`.
    ///
    /// A message that is `recency_half_life_days` old receives a recency score of `0.5`.
    /// Set `recency_weight = 0.0` to disable recency scoring entirely.
    pub recency_half_life_days: u32,
    /// Weight applied to the TF-IDF signal. Default: `0.0` (disabled).
    pub tfidf_weight: f64,
    /// Weight applied to the cognitive signal (message access frequency). Default: `0.0` (disabled).
    pub cognitive_signal_weight: f64,
    /// Weight applied to the tier boost signal for consolidated/semantic entries. Default: `0.0` (disabled).
    pub tier_boost_weight: f64,
    /// Additive score awarded to entries in the `semantic` tier when `tier_boost_weight > 0`. Default: `1.0`.
    ///
    /// The final contribution is `tier_boost_weight * semantic_tier_boost` for semantic entries
    /// and `0.0` for episodic entries.
    pub semantic_tier_boost: f64,
    /// Route the `DeepReasoning` tier graph step through query-conditioned recall (#3994).
    ///
    /// When `true`, the graph recall step for `IntentClass::DeepReasoning` uses
    /// `recall_graph_hela` (HELA spreading activation) instead of static-weight BFS,
    /// producing query-aligned results. Requires an embedding store. Default: `false` (opt-in).
    #[serde(default)]
    pub deep_reasoning_query_conditioned: bool,
}

impl Default for TieredRetrievalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            classifier_provider: ProviderName::default(),
            validator_provider: ProviderName::default(),
            token_budget: 4096,
            validation_enabled: false,
            validation_threshold: 0.6,
            max_escalations: 1,
            classifier_timeout_secs: 5,
            validator_timeout_secs: 5,
            similarity_weight: 1.0,
            recency_weight: 0.0,
            recency_half_life_days: 7,
            tfidf_weight: 0.0,
            cognitive_signal_weight: 0.0,
            tier_boost_weight: 0.0,
            semantic_tier_boost: 1.0,
            deep_reasoning_query_conditioned: false,
        }
    }
}

fn default_retrieval_failures_low_confidence_threshold() -> f32 {
    0.3
}

fn default_retrieval_failures_retention_days() -> u32 {
    90
}

fn default_retrieval_failures_channel_capacity() -> usize {
    256
}

fn default_retrieval_failures_batch_size() -> usize {
    16
}

fn default_retrieval_failures_flush_interval_ms() -> u64 {
    100
}

/// Memory snippet rendering format injected into agent context (MM-F5, #3340).
///
/// Controls how each recalled memory entry is presented in the assembled prompt.
/// Flipping this value does not affect stored content — `SQLite` rows and Qdrant points
/// always contain the raw message text. The format is applied exclusively during
/// context assembly and is never persisted.
///
/// # Token cost
///
/// `Structured` headers add roughly 2–3× more tokens per entry than `Plain`.
/// Consider raising `memory.recall_tokens` proportionally when switching to `Structured`.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContextFormat {
    /// Emit a labeled header per snippet:
    /// `[Memory | <source> | <date> | relevance: <score>]` followed by the content.
    ///
    /// This is the default. Gives the LLM structured provenance metadata for each recalled
    /// memory without re-parsing the recall body.
    #[default]
    Structured,
    /// Legacy plain format: `- [role] content` per snippet, byte-identical to pre-#3340.
    ///
    /// Use `Plain` when downstream consumers rely on the old format or when token budget
    /// is tight and provenance headers are not needed.
    Plain,
}

/// Retrieval-stage tuning for semantic memory (MemMachine-inspired, #3340).
///
/// Controls ANN candidate depth, search-prompt template, and memory snippet rendering.
/// Nested under `[memory.retrieval]` in TOML.  All fields have defaults so existing
/// configs parse unchanged.
///
/// # Example (TOML)
///
/// ```toml
/// [memory.retrieval]
/// # depth = 0          # 0 = legacy (recall_limit * 2); set ≥ 1 to override directly
/// # search_prompt_template = ""
/// # context_format = "structured"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct RetrievalConfig {
    /// Number of ANN candidates fetched from the vector store before keyword merge,
    /// temporal decay, and MMR re-ranking.
    ///
    /// - `0` (default): legacy behavior — `recall_limit * 2` candidates, byte-identical
    ///   to pre-#3340 deployments.
    /// - `≥ 1`: the configured value is passed directly to `qdrant.search` /
    ///   `keyword_search`. Set to at least `recall_limit * 2` to match the legacy pool
    ///   size, or higher for better MMR diversity.
    ///
    /// A value below `recall_limit` triggers a one-shot WARN because the ANN pool
    /// cannot saturate the requested top-k.
    pub depth: u32,
    /// Template applied to the raw user query before embedding.
    ///
    /// Supports a single `{query}` placeholder which is replaced with the raw query string.
    /// Empty string (default) = identity: the query is embedded as-is.
    ///
    /// Applied **only** at query-side embedding sites — stored content (summaries, documents)
    /// is never wrapped.  Use this for asymmetric embedding models (e.g. E5 `"query: {query}"`).
    pub search_prompt_template: String,
    /// Shape of memory snippets injected into agent context.
    ///
    /// See [`ContextFormat`] for the exact rendering and token-cost implications.
    /// Default: `Structured`.
    pub context_format: ContextFormat,
    /// Enable query-bias correction towards the user's profile centroid (MM-F3, #3341).
    ///
    /// When `true` and the query is classified as first-person, the query embedding is
    /// shifted towards the centroid of persona-fact embeddings. This nudges recall results
    /// towards persona-relevant content for self-referential queries.
    ///
    /// Default: `true` (low blast-radius: no-op when the persona table is empty).
    #[serde(default = "default_query_bias_correction")]
    pub query_bias_correction: bool,
    /// Blend weight for query-bias correction (MM-F3, #3341).
    ///
    /// Controls how much the query embedding shifts towards the profile centroid.
    /// `0.0` = no shift; `1.0` = full centroid. Clamped to `[0.0, 1.0]`. Default: `0.25`.
    #[serde(default = "default_query_bias_profile_weight")]
    pub query_bias_profile_weight: f32,
    /// Centroid TTL in seconds (MM-F3, #3341).
    ///
    /// The profile centroid computed from persona facts is cached for this many seconds.
    /// After expiry it is recomputed on the next first-person query. Default: 300 (5 min).
    #[serde(default = "default_query_bias_centroid_ttl_secs")]
    pub query_bias_centroid_ttl_secs: u64,
}

fn default_query_bias_correction() -> bool {
    true
}

fn default_query_bias_profile_weight() -> f32 {
    0.25
}

fn default_query_bias_centroid_ttl_secs() -> u64 {
    300
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            depth: 0,
            search_prompt_template: String::new(),
            context_format: ContextFormat::default(),
            query_bias_correction: default_query_bias_correction(),
            query_bias_profile_weight: default_query_bias_profile_weight(),
            query_bias_centroid_ttl_secs: default_query_bias_centroid_ttl_secs(),
        }
    }
}

fn default_consolidation_confidence_threshold() -> f32 {
    0.7
}

fn default_consolidation_sweep_interval_secs() -> u64 {
    3600
}

fn default_consolidation_sweep_batch_size() -> usize {
    50
}

fn default_consolidation_similarity_threshold() -> f32 {
    0.85
}

/// Configuration for the All-Mem lifelong memory consolidation sweep (`[memory.consolidation]`).
///
/// When `enabled = true`, a background loop periodically clusters semantically similar messages
/// and merges them into consolidated entries via an LLM call. Originals are never deleted —
/// they are marked as consolidated and deprioritized in recall via temporal decay.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ConsolidationConfig {
    /// Enable the consolidation background loop. Default: `false`.
    pub enabled: bool,
    /// Provider name from `[[llm.providers]]` for consolidation LLM calls.
    /// Falls back to the primary provider when empty. Default: `""`.
    #[serde(default)]
    pub consolidation_provider: ProviderName,
    /// Minimum LLM-assigned confidence for a topology op to be applied. Default: `0.7`.
    #[serde(default = "default_consolidation_confidence_threshold")]
    pub confidence_threshold: f32,
    /// How often the background consolidation sweep runs, in seconds. Default: `3600`.
    #[serde(default = "default_consolidation_sweep_interval_secs")]
    pub sweep_interval_secs: u64,
    /// Maximum number of messages to evaluate per sweep cycle. Default: `50`.
    #[serde(default = "default_consolidation_sweep_batch_size")]
    pub sweep_batch_size: usize,
    /// Minimum cosine similarity for two messages to be considered consolidation candidates.
    /// Default: `0.85`.
    #[serde(default = "default_consolidation_similarity_threshold")]
    pub similarity_threshold: f32,
    /// LLM call timeout per `propose_merge_op` invocation, in seconds. Default: `30`.
    #[serde(default = "default_consolidation_llm_timeout_secs")]
    pub llm_timeout_secs: u64,
    /// Per-call timeout for every `embed()` invocation in the consolidation sweep, in seconds.
    /// Default: `5`.
    #[serde(default = "default_embed_timeout_secs")]
    pub embed_timeout_secs: u64,
}

impl Default for ConsolidationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            consolidation_provider: ProviderName::default(),
            confidence_threshold: default_consolidation_confidence_threshold(),
            sweep_interval_secs: default_consolidation_sweep_interval_secs(),
            sweep_batch_size: default_consolidation_sweep_batch_size(),
            similarity_threshold: default_consolidation_similarity_threshold(),
            llm_timeout_secs: default_consolidation_llm_timeout_secs(),
            embed_timeout_secs: default_embed_timeout_secs(),
        }
    }
}

fn default_consolidation_llm_timeout_secs() -> u64 {
    30
}

fn default_admission_threshold() -> f32 {
    0.40
}

fn default_admission_fast_path_margin() -> f32 {
    0.15
}

fn default_rl_min_samples() -> u32 {
    500
}

fn default_rl_retrain_interval_secs() -> u64 {
    3600
}

/// Admission decision strategy.
///
/// `Heuristic` uses the existing multi-factor weighted score with an optional LLM call.
/// `Rl` replaces the LLM-based `future_utility` factor with a trained logistic regression model.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AdmissionStrategy {
    /// Current A-MAC behavior: weighted heuristics + optional LLM call. Default.
    #[default]
    Heuristic,
    /// Learned model: logistic regression trained on recall feedback.
    ///
    /// **Not yet wired to a runtime scorer** (#2416/#5543): no admission code path scores
    /// with a learned model today, so selecting this variant falls back to
    /// [`AdmissionStrategy::Heuristic`] scoring. `src/bootstrap/mod.rs`'s
    /// `build_admission_control` emits a `tracing::warn!` at startup when this variant is
    /// selected, so the fallback is operator-visible rather than silent.
    ///
    /// The training write path this strategy would need — `record_admission_training`,
    /// `save_rl_weights`, `load_rl_weights`, and `cleanup_old_training_data` in
    /// `zeph_memory::store::admission_training` — has no production caller, so even once a
    /// scorer is wired, there is no training data to train it on yet. The `rl_min_samples` /
    /// `rl_retrain_interval_secs` fields and the `admission_training_data` /
    /// `admission_rl_weights` storage tables remain as scaffolding for that future work.
    Rl,
}

fn validate_admission_weight<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <f32 as serde::Deserialize>::deserialize(deserializer)?;
    if value < 0.0 {
        return Err(serde::de::Error::custom(
            "admission weight must be non-negative (>= 0.0)",
        ));
    }
    Ok(value)
}

/// Per-factor weights for the A-MAC admission score (`[memory.admission.weights]`).
///
/// Weights are normalized at runtime (divided by their sum), so they do not need to sum to 1.0.
/// All values must be non-negative.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AdmissionWeights {
    /// LLM-estimated future reuse probability. Default: `0.30`.
    #[serde(deserialize_with = "validate_admission_weight")]
    pub future_utility: f32,
    /// Factual confidence heuristic (inverse of hedging markers). Default: `0.15`.
    #[serde(deserialize_with = "validate_admission_weight")]
    pub factual_confidence: f32,
    /// Semantic novelty: 1 - max similarity to existing memories. Default: `0.30`.
    #[serde(deserialize_with = "validate_admission_weight")]
    pub semantic_novelty: f32,
    /// Temporal recency: always 1.0 at write time. Default: `0.10`.
    #[serde(deserialize_with = "validate_admission_weight")]
    pub temporal_recency: f32,
    /// Content type prior based on role. Default: `0.15`.
    #[serde(deserialize_with = "validate_admission_weight")]
    pub content_type_prior: f32,
    /// Goal-conditioned utility (#2408). `0.0` when `goal_conditioned_write = false`.
    /// When enabled, set this alongside reducing `future_utility` so total sums remain stable.
    /// Normalized automatically at runtime. Default: `0.0`.
    #[serde(deserialize_with = "validate_admission_weight")]
    pub goal_utility: f32,
}

impl Default for AdmissionWeights {
    fn default() -> Self {
        Self {
            future_utility: 0.30,
            factual_confidence: 0.15,
            semantic_novelty: 0.30,
            temporal_recency: 0.10,
            content_type_prior: 0.15,
            goal_utility: 0.0,
        }
    }
}

impl AdmissionWeights {
    /// Return weights normalized so they sum to 1.0.
    ///
    /// All weights are non-negative; the sum is always > 0 when defaults are used.
    #[must_use]
    pub fn normalized(&self) -> Self {
        let sum = self.future_utility
            + self.factual_confidence
            + self.semantic_novelty
            + self.temporal_recency
            + self.content_type_prior
            + self.goal_utility;
        if sum <= f32::EPSILON {
            return Self::default();
        }
        Self {
            future_utility: self.future_utility / sum,
            factual_confidence: self.factual_confidence / sum,
            semantic_novelty: self.semantic_novelty / sum,
            temporal_recency: self.temporal_recency / sum,
            content_type_prior: self.content_type_prior / sum,
            goal_utility: self.goal_utility / sum,
        }
    }
}

/// Configuration for A-MAC adaptive memory admission control (`[memory.admission]` TOML section).
///
/// When `enabled = true`, a write-time gate evaluates each message before saving to memory.
/// Messages below the composite admission threshold are rejected and not persisted.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AdmissionConfig {
    /// Enable A-MAC admission control. Default: `false`.
    pub enabled: bool,
    /// Composite score threshold below which messages are rejected. Range: `[0.0, 1.0]`.
    /// Default: `0.40`.
    #[serde(deserialize_with = "crate::de_helpers::de_unit_closed")]
    pub threshold: f32,
    /// Margin above threshold at which the fast path admits without an LLM call. Range: `[0.0, 1.0]`.
    /// When heuristic score >= threshold + margin, LLM call is skipped. Default: `0.15`.
    #[serde(deserialize_with = "crate::de_helpers::de_unit_closed")]
    pub fast_path_margin: f32,
    /// Provider name from `[[llm.providers]]` for `future_utility` LLM evaluation.
    /// Falls back to the primary provider when empty. Default: `""`.
    pub admission_provider: ProviderName,
    /// Per-factor weights. Normalized at runtime. Default: `{0.30, 0.15, 0.30, 0.10, 0.15}`.
    pub weights: AdmissionWeights,
    /// Admission decision strategy. Default: `heuristic`.
    #[serde(default)]
    pub admission_strategy: AdmissionStrategy,
    /// Minimum training samples before the RL model is activated.
    /// Below this count the system falls back to `Heuristic`. Default: `500`.
    #[serde(default = "default_rl_min_samples")]
    pub rl_min_samples: u32,
    /// Background RL model retraining interval in seconds. Default: `3600`.
    #[serde(default = "default_rl_retrain_interval_secs")]
    pub rl_retrain_interval_secs: u64,
    /// Enable goal-conditioned write gate (#2408). When `true`, memories are scored
    /// against the current task goal and rejected if relevance is below `goal_utility_threshold`.
    /// Zero regression when `false`. Default: `false`.
    #[serde(default)]
    pub goal_conditioned_write: bool,
    /// Provider name from `[[llm.providers]]` for goal-utility LLM refinement.
    /// Used only for borderline cases (similarity within 0.1 of threshold).
    /// Falls back to the primary provider when empty. Default: `""`.
    #[serde(default)]
    pub goal_utility_provider: ProviderName,
    /// Minimum cosine similarity between goal embedding and candidate memory
    /// to consider it goal-relevant. Below this, `goal_utility = 0.0`. Default: `0.4`.
    #[serde(default = "default_goal_utility_threshold")]
    pub goal_utility_threshold: f32,
    /// Weight of the `goal_utility` factor in the composite admission score.
    /// Set to `0.0` to disable (equivalent to `goal_conditioned_write = false`). Default: `0.25`.
    #[serde(default = "default_goal_utility_weight")]
    pub goal_utility_weight: f32,
}

fn default_goal_utility_threshold() -> f32 {
    0.4
}

fn default_goal_utility_weight() -> f32 {
    0.25
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: default_admission_threshold(),
            fast_path_margin: default_admission_fast_path_margin(),
            admission_provider: ProviderName::default(),
            weights: AdmissionWeights::default(),
            admission_strategy: AdmissionStrategy::default(),
            rl_min_samples: default_rl_min_samples(),
            rl_retrain_interval_secs: default_rl_retrain_interval_secs(),
            goal_conditioned_write: false,
            goal_utility_provider: ProviderName::default(),
            goal_utility_threshold: default_goal_utility_threshold(),
            goal_utility_weight: default_goal_utility_weight(),
        }
    }
}

/// Routing strategy for `[memory.store_routing]`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StoreRoutingStrategy {
    /// Pure heuristic pattern matching. Zero LLM calls. Default.
    #[default]
    Heuristic,
    /// LLM-based classification via `routing_classifier_provider`.
    Llm,
    /// Heuristic first; escalates to LLM only when confidence is low.
    Hybrid,
}

/// Configuration for cost-sensitive store routing (`[memory.store_routing]`).
///
/// Controls how each query is classified and routed to the appropriate memory
/// backend(s), avoiding unnecessary store queries for simple lookups.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct StoreRoutingConfig {
    /// Enable configurable store routing. When `false`, `HeuristicRouter` is used
    /// directly (existing behavior). Default: `false`.
    pub enabled: bool,
    /// Routing strategy. Default: `heuristic`.
    pub strategy: StoreRoutingStrategy,
    /// Provider name from `[[llm.providers]]` for LLM-based classification.
    /// Falls back to the primary provider when empty. Default: `""`.
    pub routing_classifier_provider: ProviderName,
    /// Route to use when the classifier is uncertain (confidence < threshold).
    ///
    /// Defaults to [`MemoryRoute::Hybrid`].
    pub fallback_route: MemoryRoute,
    /// Confidence threshold below which `HybridRouter` escalates to LLM.
    /// Range: `[0.0, 1.0]`. Default: `0.7`.
    pub confidence_threshold: f32,
}

impl Default for StoreRoutingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            strategy: StoreRoutingStrategy::Heuristic,
            routing_classifier_provider: ProviderName::default(),
            fallback_route: MemoryRoute::Hybrid,
            confidence_threshold: 0.7,
        }
    }
}

/// `OmniMem` retrieval failure tracking configuration (issue #3576).
///
/// Controls the async logger that records no-hit and low-confidence recall events
/// to `memory_retrieval_failures` for closed-loop memory parameter tuning.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct RetrievalFailuresConfig {
    /// Enable retrieval failure logging. Default: `false`.
    pub enabled: bool,
    /// Composite recall score below which a result is classified as low-confidence.
    ///
    /// The threshold applies to the post-reranking composite score (which incorporates
    /// MMR, temporal decay, importance weighting, and tier boost). Calibrate against
    /// the scoring pipeline in use. Default: `0.3`.
    #[serde(default = "default_retrieval_failures_low_confidence_threshold")]
    pub low_confidence_threshold: f32,
    /// Days to retain failure records before automatic cleanup. Default: `90`.
    #[serde(default = "default_retrieval_failures_retention_days")]
    pub retention_days: u32,
    /// Bounded mpsc channel capacity for the fire-and-forget write path. Default: `256`.
    #[serde(default = "default_retrieval_failures_channel_capacity")]
    pub channel_capacity: usize,
    /// Maximum records collected before flushing a batch INSERT. Default: `16`.
    #[serde(default = "default_retrieval_failures_batch_size")]
    pub batch_size: usize,
    /// Maximum milliseconds to wait before flushing a partial batch. Default: `100`.
    #[serde(default = "default_retrieval_failures_flush_interval_ms")]
    pub flush_interval_ms: u64,
}

impl Default for RetrievalFailuresConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            low_confidence_threshold: default_retrieval_failures_low_confidence_threshold(),
            retention_days: default_retrieval_failures_retention_days(),
            channel_capacity: default_retrieval_failures_channel_capacity(),
            batch_size: default_retrieval_failures_batch_size(),
            flush_interval_ms: default_retrieval_failures_flush_interval_ms(),
        }
    }
}
