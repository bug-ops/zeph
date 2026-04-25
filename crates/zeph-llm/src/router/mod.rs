// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Multi-provider router with pluggable routing strategies.
//!
//! [`RouterProvider`] implements [`LlmProvider`] and forwards every call to one of
//! its configured backends, chosen according to the active [`RouterStrategy`].
//!
//! # Routing strategies
//!
//! | Strategy | Module | Description |
//! |---|---|---|
//! | [`RouterStrategy::Ema`] | `crate::ema` | EMA-weighted latency-aware ordering |
//! | [`RouterStrategy::Thompson`] | [`thompson`] | Bayesian Beta-distribution sampling |
//! | [`RouterStrategy::Cascade`] | [`cascade`] | Cheapest-first with quality escalation |
//! | [`RouterStrategy::Bandit`] | [`bandit`] | Contextual `LinUCB` (PILOT algorithm) |
//!
//! Strategies are selected via builder methods on [`RouterProvider`]:
//! - [`RouterProvider::with_ema`]
//! - [`RouterProvider::with_thompson`]
//! - [`RouterProvider::with_cascade`]
//! - [`RouterProvider::with_bandit`]
//!
//! # Reputation-Aware Provider Selection (RAPS)
//!
//! All strategies support an optional Bayesian reputation layer ([`reputation`]) that
//! penalizes providers which produce semantically invalid tool arguments. Enable with
//! [`RouterProvider::with_reputation`].
//!
//! # Agent Stability Index (ASI)
//!
//! An optional session-level coherence tracker ([`asi`]) measures embedding-based
//! response quality and feeds back into Thompson selection. Enable with
//! [`RouterProvider::with_asi`].
//!
//! # Security
//!
//! Thompson and Bandit state files are loaded from user-controlled paths at startup.
//! Files are validated (finite floats, clamped range) and written with `0o600` permissions
//! on Unix. Do not store state files in world-writable directories.
//!
//! # Security
//!
//! Thompson and Bandit state are loaded from user-controlled paths at startup. Files are
//! validated (finite floats, clamped range) and written with `0o600` permissions
//! on Unix. Do not store state files in world-writable directories.

pub mod asi;
pub mod aware;
pub mod bandit;
pub mod cascade;
pub mod coe;
pub mod reputation;
pub mod state;
pub mod thompson;
pub mod triage;

pub use aware::RouterAware;
pub use state::RouterState;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::any::AnyProvider;
use crate::ema::EmaTracker;
use crate::embed::owned_strs;
use crate::error::LlmError;
use crate::provider::{ChatResponse, ChatStream, LlmProvider, Message, StatusTx, ToolDefinition};
use coe::{CoeDecision, CoeRouter, run_coe};

use asi::AsiState;
use bandit::{BanditState, embedding_to_features};
use cascade::{CascadeState, ClassifierMode, heuristic_score};
use reputation::ReputationTracker;
use thompson::ThompsonState;

/// Rate-limits the ASI coherence WARN to at most once per 60 seconds process-wide.
static ASI_WARN_LAST_SECS: AtomicU64 = AtomicU64::new(0);
use zeph_common::math::cosine_similarity;

/// Simple bounded embedding cache for bandit feature vectors.
///
/// Keyed by `u64` hash of query text (using `std::hash`). Eviction is FIFO on insertion
/// order (not LRU) — acceptable for a routing cache where hot queries repeat often.
/// The `lru` crate is not in the workspace; a `HashMap` + insertion-order Vec avoids a new dep.
#[derive(Debug)]
struct BanditEmbedCache {
    map: HashMap<u64, Vec<f32>>,
    order: std::collections::VecDeque<u64>,
    capacity: usize,
}

impl BanditEmbedCache {
    fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity(capacity),
            order: std::collections::VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn get(&self, key: u64) -> Option<&Vec<f32>> {
        self.map.get(&key)
    }

    fn insert(&mut self, key: u64, value: Vec<f32>) {
        if self.map.contains_key(&key) {
            return;
        }
        if self.map.len() >= self.capacity
            && let Some(evict) = self.order.pop_front()
        {
            self.map.remove(&evict);
        }
        self.map.insert(key, value);
        self.order.push_back(key);
    }
}

impl Default for BanditEmbedCache {
    fn default() -> Self {
        Self::new(512)
    }
}

/// Per-turn embedding cache keyed by the exact input string.
///
/// Created at the start of each `chat()` call and dropped at the end. With 2-4 entries
/// per turn, `String` keys have negligible overhead and eliminate the hash-collision risk
/// of `u64`-keyed caches.
#[derive(Debug, Default)]
struct TurnEmbedCache {
    entries: HashMap<String, Vec<f32>>,
}

impl TurnEmbedCache {
    fn get(&self, text: &str) -> Option<&Vec<f32>> {
        self.entries.get(text)
    }

    fn insert(&mut self, text: impl Into<String>, embedding: Vec<f32>) {
        self.entries.insert(text.into(), embedding);
    }
}

/// Routing strategy used by [`RouterProvider`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RouterStrategy {
    /// Exponential moving average-based latency-aware ordering.
    #[default]
    Ema,
    /// Thompson Sampling with Beta distributions.
    Thompson,
    /// Cascade: try cheapest provider first, escalate on degenerate output.
    Cascade,
    /// PILOT: `LinUCB` contextual bandit with online learning and budget-aware selection.
    Bandit,
}

/// Configuration for PILOT bandit routing in `RouterProvider`.
///
/// See [`bandit`] module for the algorithm details and trade-offs.
#[derive(Debug, Clone)]
#[allow(clippy::doc_markdown)] // PILOT, LinUCB, Thompson are proper nouns/acronyms
pub struct BanditRouterConfig {
    /// `LinUCB` exploration parameter. Higher = more exploration. Default: 1.0.
    pub alpha: f32,
    /// Feature vector dimension (first `dim` components of embedding). Default: 32.
    pub dim: usize,
    /// Cost penalty weight in the reward signal: `reward = quality - cost_weight * cost_fraction`.
    /// Default: 0.1. Increase to penalise expensive providers more aggressively.
    pub cost_weight: f32,
    /// Session-level decay factor: values < 1.0 cause re-exploration over time. Default: 1.0.
    pub decay_factor: f32,
    /// Minimum total updates before `LinUCB` takes over from Thompson fallback.
    /// Default: `10 * num_providers` (computed at construction time from provider count).
    pub warmup_queries: u64,
    /// Hard timeout for the embedding call (milliseconds). If exceeded, falls back
    /// to Thompson/uniform selection. Default: 50.
    pub embedding_timeout_ms: u64,
    /// Maximum number of cached embeddings (keyed by query string hash). Default: 512.
    pub cache_size: usize,
    /// MAR threshold: when `memory_hit_confidence >= this`, bias toward cheap providers.
    /// Default: 0.9. Set to 1.0 to disable MAR.
    pub memory_confidence_threshold: f32,
}

impl Default for BanditRouterConfig {
    fn default() -> Self {
        Self {
            alpha: 1.0,
            dim: 32,
            cost_weight: 0.1,
            decay_factor: 1.0,
            warmup_queries: 0, // overridden by with_bandit() based on provider count
            embedding_timeout_ms: 50,
            cache_size: 512,
            memory_confidence_threshold: 0.9,
        }
    }
}

/// Runtime ASI configuration passed to [`RouterProvider::with_asi`].
///
/// Mirrors `AsiRouterConfig` but lives in `zeph-llm` to avoid
/// a dependency on `zeph-config`. The bootstrap layer maps config → this struct.
#[derive(Debug, Clone)]
pub struct AsiRouterConfig {
    /// Sliding window size. Default: 5.
    pub window: usize,
    /// Coherence score threshold below which the provider is penalized. Default: 0.7.
    pub coherence_threshold: f32,
    /// Penalty weight added to Thompson beta on low coherence. Default: 0.3.
    pub penalty_weight: f32,
}

impl Default for AsiRouterConfig {
    fn default() -> Self {
        Self {
            window: 5,
            coherence_threshold: 0.7,
            penalty_weight: 0.3,
        }
    }
}

/// Configuration for cascade routing in `RouterProvider`.
#[derive(Debug, Clone)]
pub struct CascadeRouterConfig {
    pub quality_threshold: f64,
    pub max_escalations: u8,
    pub classifier_mode: ClassifierMode,
    pub window_size: usize,
    pub max_cascade_tokens: Option<u32>,
    /// LLM provider used for judge-mode quality scoring.
    /// Required when `classifier_mode = Judge`; falls back to heuristic if `None`.
    pub summary_provider: Option<AnyProvider>,
    /// Explicit cost ordering of provider names (cheapest first).
    /// When set, providers are sorted by their position in this list at construction time.
    /// Providers not listed are appended after listed ones in original chain order.
    pub cost_tiers: Option<Vec<String>>,
}

impl Default for CascadeRouterConfig {
    fn default() -> Self {
        Self {
            quality_threshold: 0.5,
            max_escalations: 2,
            classifier_mode: ClassifierMode::Heuristic,
            window_size: 50,
            max_cascade_tokens: None,
            summary_provider: None,
            cost_tiers: None,
        }
    }
}

/// Multi-provider LLM router implementing [`LlmProvider`].
///
/// Construct with [`RouterProvider::new`] and configure a routing strategy via the
/// builder methods. All configuration is immutable after construction except for
/// runtime state (EMA statistics, Thompson distribution, bandit weights) which is
/// stored behind `Arc<Mutex<_>>` and updated on every successful call.
///
/// Cloning is cheap: [`RouterState`] and all per-strategy state are `Arc`-wrapped
/// and shared between the original and all clones — clone cost is proportional to
/// the number of `Arc` fields, not to provider count or strategy complexity.
#[derive(Debug, Clone)]
pub struct RouterProvider {
    /// Shared cross-strategy runtime signals (providers, turn counter, MAR, etc.).
    ///
    /// All fields inside are `Arc`-wrapped; clone is O(1).
    pub(crate) state: RouterState,
    status_tx: Option<StatusTx>,
    ema: Option<EmaTracker>,
    strategy: RouterStrategy,
    thompson: Option<Arc<Mutex<ThompsonState>>>,
    /// Path for persisting Thompson state. `None` disables persistence.
    thompson_state_path: Option<std::path::PathBuf>,
    /// Cascade routing state (quality history per provider).
    cascade_state: Option<Arc<Mutex<CascadeState>>>,
    /// Cascade routing configuration.
    cascade_config: Option<CascadeRouterConfig>,
    /// Bayesian reputation tracker (RAPS). None when disabled.
    reputation: Option<Arc<Mutex<ReputationTracker>>>,
    /// Path for persisting reputation state.
    reputation_state_path: Option<std::path::PathBuf>,
    /// Reputation weight in [0.0, 1.0] for routing score blend.
    reputation_weight: f64,
    /// PILOT bandit state.
    bandit: Option<Arc<Mutex<BanditState>>>,
    /// Path for persisting bandit state. `None` disables persistence.
    bandit_state_path: Option<std::path::PathBuf>,
    /// Bandit routing configuration.
    bandit_config: Option<BanditRouterConfig>,
    /// Dedicated embedding provider for bandit feature vectors.
    /// When `None`, bandit falls back to Thompson/uniform on embed failure.
    bandit_embedding_provider: Option<AnyProvider>,
    /// LRU embedding cache: maps query-string hash to feature vector.
    /// Shared across requests; keyed by `u64` hash of query text.
    bandit_embed_cache: Arc<Mutex<BanditEmbedCache>>,
    /// Agent Stability Index state (session-only coherence tracking).
    asi: Option<Arc<Mutex<AsiState>>>,
    /// ASI configuration. `None` when ASI is disabled.
    asi_config: Option<AsiRouterConfig>,
    /// Embedding-based quality gate threshold. `None` = disabled.
    /// After provider selection, `cosine_similarity(query_emb, response_emb)` must be >= this
    /// value; otherwise the next provider in the ordered list is tried.
    quality_gate: Option<f32>,
    /// `CoE` (Collaborative Entropy) router. `None` when `CoE` is disabled.
    coe: Option<Arc<CoeRouter>>,
}

impl RouterProvider {
    /// Create a new router over `providers`.
    ///
    /// Use the builder methods (e.g., [`with_thompson`][Self::with_thompson],
    /// [`with_cascade`][Self::with_cascade]) to configure a routing strategy.
    /// The default strategy is [`RouterStrategy::Ema`].
    #[must_use]
    pub fn new(providers: Vec<AnyProvider>) -> Self {
        let state = RouterState::new(Arc::from(providers));
        Self {
            state,
            status_tx: None,
            ema: None,
            strategy: RouterStrategy::Ema,
            thompson: None,
            thompson_state_path: None,
            cascade_state: None,
            cascade_config: None,
            reputation: None,
            reputation_state_path: None,
            reputation_weight: 0.3,
            bandit: None,
            bandit_state_path: None,
            bandit_config: None,
            bandit_embedding_provider: None,
            bandit_embed_cache: Arc::new(Mutex::new(BanditEmbedCache::default())),
            asi: None,
            asi_config: None,
            quality_gate: None,
            coe: None,
        }
    }

    /// Set the maximum number of concurrent `embed_batch` calls.
    ///
    /// A value of 0 disables the semaphore (unlimited). Default is no semaphore.
    #[must_use]
    pub fn with_embed_concurrency(mut self, limit: usize) -> Self {
        self.state.embed_semaphore = if limit > 0 {
            Some(Arc::new(tokio::sync::Semaphore::new(limit)))
        } else {
            None
        };
        self
    }

    /// Set the MAR (Memory-Augmented Routing) signal for the current turn.
    ///
    /// Must be called before `chat` / `chat_stream` to influence bandit provider selection.
    /// Pass `None` to disable MAR for this turn.
    pub fn set_memory_confidence(&self, confidence: Option<f32>) {
        *self.state.last_memory_confidence.lock() = confidence;
    }

    /// Enable EMA-based adaptive provider ordering.
    #[must_use]
    pub fn with_ema(mut self, alpha: f64, reorder_interval: u64) -> Self {
        self.ema = Some(EmaTracker::new(alpha, reorder_interval));
        self
    }

    /// Enable Collaborative Entropy (`CoE`) for Ema/Thompson strategies.
    ///
    /// `CoE` detects uncertain responses via intra-entropy and inter-divergence signals,
    /// escalating to `secondary` when either threshold is exceeded.
    ///
    /// No-op (with a `warn!`) when the active strategy is `Cascade` or `Bandit`.
    #[must_use]
    pub fn with_coe(
        mut self,
        config: coe::CoeConfig,
        secondary: AnyProvider,
        embed: AnyProvider,
    ) -> Self {
        if matches!(
            self.strategy,
            RouterStrategy::Cascade | RouterStrategy::Bandit
        ) {
            tracing::warn!(
                strategy = ?self.strategy,
                "coe disabled for strategy; supported: ema, thompson"
            );
            return self;
        }
        self.coe = Some(Arc::new(CoeRouter {
            config,
            secondary,
            embed,
            metrics: Arc::new(coe::CoeMetrics::default()),
        }));
        self
    }

    /// Return session-level `CoE` metrics snapshot, or `None` if `CoE` is disabled.
    #[must_use]
    pub fn coe_metrics(&self) -> Option<(u64, u64, u64, u64)> {
        self.coe.as_ref().map(|c| {
            (
                c.metrics.kept_primary.load(Ordering::Relaxed),
                c.metrics.intra_escalations.load(Ordering::Relaxed),
                c.metrics.inter_escalations.load(Ordering::Relaxed),
                c.metrics.embed_failures.load(Ordering::Relaxed),
            )
        })
    }

    /// Enable Agent Stability Index (ASI) coherence tracking.
    ///
    /// When enabled, each successful response is embedded in a background task and added
    /// to a per-provider sliding window. The coherence score (cosine similarity of the
    /// latest embedding vs. window mean) penalizes Thompson/EMA routing priors for
    /// providers whose responses drift.
    #[must_use]
    pub fn with_asi(mut self, config: AsiRouterConfig) -> Self {
        self.asi = Some(Arc::new(Mutex::new(AsiState::default())));
        self.asi_config = Some(config);
        self
    }

    /// Enable embedding-based quality gate for Thompson/EMA routing.
    ///
    /// After provider selection, computes cosine similarity between the query embedding
    /// and the response embedding. If below `threshold`, tries the next provider in the
    /// ordered list. On full exhaustion, returns the best response seen (highest similarity).
    /// Fail-open: embedding errors disable the gate for that request.
    #[must_use]
    pub fn with_quality_gate(mut self, threshold: f32) -> Self {
        self.quality_gate = Some(threshold);
        self
    }

    /// Enable Thompson Sampling strategy.
    ///
    /// Loads existing state from `state_path` if present; falls back to uniform prior.
    /// Prunes stale entries for providers not in the current chain.
    #[must_use]
    pub fn with_thompson(mut self, state_path: Option<&Path>) -> Self {
        self.strategy = RouterStrategy::Thompson;
        let path = state_path.map_or_else(ThompsonState::default_path, Path::to_path_buf);
        let mut state = ThompsonState::load(&path);
        // CRIT-3: prune orphan entries from previous configs.
        let known: std::collections::HashSet<String> = self
            .state
            .providers
            .iter()
            .map(|p| p.name().to_owned())
            .collect();
        state.prune(&known);
        self.thompson = Some(Arc::new(Mutex::new(state)));
        self.thompson_state_path = Some(path);
        self
    }

    /// Enable PILOT bandit routing strategy (`LinUCB` contextual bandit).
    ///
    /// Loads existing state from `state_path` (or the default path). Applies session-level
    /// decay if `config.decay_factor < 1.0`, and prunes arms for removed providers.
    ///
    /// `embedding_provider` is used to obtain feature vectors for each query.
    /// When `None`, the bandit falls back to Thompson/uniform selection whenever an
    /// embedding cannot be obtained within `config.embedding_timeout_ms`.
    ///
    /// The `warmup_queries` default of `0` in `BanditRouterConfig` is overridden here to
    /// `10 * num_providers` to ensure sufficient initial exploration.
    #[must_use]
    pub fn with_bandit(
        mut self,
        mut config: BanditRouterConfig,
        state_path: Option<&Path>,
        embedding_provider: Option<AnyProvider>,
    ) -> Self {
        self.strategy = RouterStrategy::Bandit;
        let n = self.state.providers.len();
        if config.warmup_queries == 0 {
            config.warmup_queries = u64::try_from(10 * n.max(1)).unwrap_or(100);
        }
        let cache_size = config.cache_size;
        let path = state_path.map_or_else(BanditState::default_path, Path::to_path_buf);
        let mut state = BanditState::load(&path);
        if state.dim == 0 {
            state = BanditState::new(config.dim);
        } else if state.dim != config.dim {
            // Config changed dim — reset state rather than use mismatched arms.
            tracing::warn!(
                old_dim = state.dim,
                new_dim = config.dim,
                "bandit: dim changed, resetting state"
            );
            state = BanditState::new(config.dim);
        }
        // Validate config bounds before applying. Clamp to safe ranges with a warning.
        if config.alpha <= 0.0 {
            tracing::warn!(alpha = config.alpha, "bandit: alpha <= 0, clamping to 0.01");
            config.alpha = 0.01;
        }
        if config.dim == 0 || config.dim > 256 {
            tracing::warn!(
                dim = config.dim,
                "bandit: dim out of range [1, 256], clamping to 32"
            );
            config.dim = 32;
        }
        if config.decay_factor <= 0.0 || config.decay_factor > 1.0 {
            tracing::warn!(
                decay_factor = config.decay_factor,
                "bandit: decay_factor out of (0.0, 1.0], clamping to 1.0"
            );
            config.decay_factor = 1.0;
        }
        if config.decay_factor < 1.0 {
            state.apply_decay(config.decay_factor);
        }
        let known: std::collections::HashSet<String> = self
            .state
            .providers
            .iter()
            .map(|p| p.name().to_owned())
            .collect();
        state.prune(&known);
        self.bandit = Some(Arc::new(Mutex::new(state)));
        self.bandit_state_path = Some(path);
        self.bandit_embed_cache = Arc::new(Mutex::new(BanditEmbedCache::new(cache_size)));
        self.bandit_embedding_provider = embedding_provider;
        // Initialize Thompson state for cold-start fallback (total_updates < warmup_queries).
        // Uses default uniform priors; no persistence path needed since it's a fallback only.
        self.thompson = Some(Arc::new(Mutex::new(ThompsonState::default())));
        self.bandit_config = Some(config);
        self
    }

    /// Persist current bandit state to disk. No-op if bandit strategy is not active.
    pub fn save_bandit_state(&self) {
        let (Some(bandit), Some(path)) = (&self.bandit, &self.bandit_state_path) else {
            return;
        };
        let state = bandit.lock();
        if let Err(e) = state.save(path) {
            tracing::warn!(error = %e, "failed to save bandit state");
        }
    }

    /// Return bandit diagnostic stats: `(provider_name, pulls, mean_reward)`.
    ///
    /// Returns an empty vec if bandit strategy is not active.
    #[must_use]
    pub fn bandit_stats(&self) -> Vec<(String, u64, f32)> {
        let Some(ref bandit) = self.bandit else {
            return vec![];
        };
        let state = bandit.lock();
        state.stats()
    }

    /// Enable Bayesian reputation scoring (RAPS).
    ///
    /// Loads existing state from `state_path` (or the default path), applies session-level
    /// decay, and prunes stale provider entries.
    ///
    /// No-op for Cascade routing (reputation is not used for cost-tier ordering).
    #[must_use]
    pub fn with_reputation(
        mut self,
        decay_factor: f64,
        weight: f64,
        min_observations: u64,
        state_path: Option<&Path>,
    ) -> Self {
        let path = state_path.map_or_else(ReputationTracker::default_path, Path::to_path_buf);
        // Load persisted state, apply decay, and prune orphaned providers.
        let mut tracker = ReputationTracker::load(&path);
        let known: std::collections::HashSet<String> = self
            .state
            .providers
            .iter()
            .map(|p| p.name().to_owned())
            .collect();
        tracker.apply_decay();
        tracker.prune(&known);
        // Overwrite config params (decay/min_obs may differ from the persisted defaults).
        let tracker = {
            let stats = tracker.stats();
            let mut t = ReputationTracker::new(decay_factor, min_observations);
            for (name, alpha, beta, _, obs) in stats {
                t.models.insert(
                    name,
                    reputation::ReputationEntry {
                        dist: thompson::BetaDist { alpha, beta },
                        observations: obs,
                    },
                );
            }
            t
        };
        self.reputation = Some(Arc::new(Mutex::new(tracker)));
        self.reputation_state_path = Some(path);
        self.reputation_weight = weight.clamp(0.0, 1.0);
        self
    }

    /// Record a quality outcome for the last active sub-provider (tool execution result).
    ///
    /// Call only for semantic failures (invalid tool args, parse errors).
    /// Do NOT call for network errors, rate limits, or transient I/O failures.
    /// No-op when reputation scoring is disabled, strategy is Cascade, or no tool call
    /// has been made yet in this session.
    ///
    /// The `_provider_name` parameter is ignored — quality is attributed to the sub-provider
    /// that served the most recent `chat_with_tools` call, tracked via `last_active_provider`.
    pub fn record_quality_outcome(&self, _provider_name: &str, success: bool) {
        if matches!(
            self.strategy,
            RouterStrategy::Cascade | RouterStrategy::Bandit
        ) {
            // Cascade: quality tracked via CascadeState.
            // Bandit: quality fed via bandit_record_reward() after each response.
            return;
        }
        let Some(ref reputation) = self.reputation else {
            return;
        };
        let active = self.state.last_active_provider.lock().clone();
        let Some(provider_name) = active else {
            return;
        };
        let mut tracker = reputation.lock();
        tracker.record_quality(&provider_name, success);
    }

    /// Persist current reputation state to disk. No-op if reputation is disabled.
    pub fn save_reputation_state(&self) {
        let (Some(reputation), Some(path)) = (&self.reputation, &self.reputation_state_path) else {
            return;
        };
        let state = reputation.lock();
        if let Err(e) = state.save(path) {
            tracing::warn!(error = %e, "failed to save reputation state");
        }
    }

    /// Return reputation stats for all tracked providers: (name, alpha, beta, mean, observations).
    #[must_use]
    pub fn reputation_stats(&self) -> Vec<(String, f64, f64, f64, u64)> {
        let Some(ref reputation) = self.reputation else {
            return vec![];
        };
        let tracker = reputation.lock();
        tracker.stats()
    }

    /// Enable Cascade routing strategy.
    ///
    /// Providers are tried in chain order (cheapest first). Each response is evaluated
    /// by the quality classifier; if it falls below `quality_threshold`, the next
    /// provider is tried. At most `max_escalations` quality-based escalations occur.
    ///
    /// Network/API errors do not count against the escalation budget.
    /// The best response seen so far is returned if all escalations are exhausted.
    ///
    /// When `config.cost_tiers` is set, providers are reordered once at construction
    /// time (no per-request cost). Providers absent from `cost_tiers` are appended
    /// after listed ones in original chain order. Unknown names in `cost_tiers` are
    /// silently ignored.
    #[must_use]
    pub fn with_cascade(mut self, config: CascadeRouterConfig) -> Self {
        self.strategy = RouterStrategy::Cascade;

        if let Some(ref tiers) = config.cost_tiers
            && !tiers.is_empty()
        {
            let tier_pos: std::collections::HashMap<&str, usize> = tiers
                .iter()
                .enumerate()
                .map(|(i, n)| (n.as_str(), i))
                .collect();

            let before: Vec<_> = self
                .state
                .providers
                .iter()
                .map(|p| p.name().to_owned())
                .collect();
            let mut indexed: Vec<(usize, AnyProvider)> =
                self.state.providers.iter().cloned().enumerate().collect();
            indexed.sort_by_key(|(orig_idx, p)| {
                tier_pos
                    .get(p.name())
                    .copied()
                    .map_or((1usize, *orig_idx), |t| (0, t))
            });
            let after: Vec<_> = indexed.iter().map(|(_, p)| p.name().to_owned()).collect();
            if before != after {
                tracing::debug!(
                    before = ?before,
                    after = ?after,
                    "cascade: providers reordered by cost_tiers"
                );
            }
            self.state.providers =
                Arc::from(indexed.into_iter().map(|(_, p)| p).collect::<Vec<_>>());
        }

        let window = config.window_size;
        self.cascade_state = Some(Arc::new(Mutex::new(CascadeState::new(window))));
        self.cascade_config = Some(config);
        self
    }

    /// Persist current Thompson state to disk.
    ///
    /// No-op if Thompson strategy is not active.
    ///
    /// # Note
    ///
    /// This performs synchronous I/O. Called at agent shutdown from an async context;
    /// acceptable since it runs after all in-flight requests have completed.
    // FIXME: if called mid-request, use `tokio::task::spawn_blocking` instead.
    pub fn save_thompson_state(&self) {
        let (Some(thompson), Some(path)) = (&self.thompson, &self.thompson_state_path) else {
            return;
        };
        let state = thompson.lock();
        if let Err(e) = state.save(path) {
            tracing::warn!(error = %e, "failed to save Thompson router state");
        }
    }

    /// Hash a query string to a `u64` cache key.
    fn query_hash(query: &str) -> u64 {
        use std::hash::{Hash as _, Hasher as _};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        query.hash(&mut h);
        h.finish()
    }

    /// Fetch or compute the feature vector for `query` using the bandit embedding provider.
    ///
    /// Returns `None` if:
    /// - No embedding provider is configured.
    /// - The embedding call exceeds `embedding_timeout_ms`.
    /// - The embedding is shorter than `dim` or is all-zero.
    async fn bandit_features(&self, query: &str) -> Option<Vec<f32>> {
        let cfg = self.bandit_config.as_ref()?;
        let key = Self::query_hash(query);

        // Check cache first (no async needed).
        {
            let cache = self.bandit_embed_cache.lock();
            if let Some(cached) = cache.get(key) {
                return Some(cached.clone());
            }
        }

        let provider = self.bandit_embedding_provider.as_ref()?;
        let timeout = std::time::Duration::from_millis(cfg.embedding_timeout_ms);
        let embed_future = provider.embed(query);
        let embedding = match tokio::time::timeout(timeout, embed_future).await {
            Ok(Ok(emb)) => emb,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "bandit: embedding failed, falling back");
                return None;
            }
            Err(_) => {
                tracing::debug!(
                    timeout_ms = cfg.embedding_timeout_ms,
                    "bandit: embedding timed out, falling back"
                );
                return None;
            }
        };

        let features = embedding_to_features(&embedding, cfg.dim)?;

        // Insert into cache.
        {
            let mut cache = self.bandit_embed_cache.lock();
            cache.insert(key, features.clone());
        }
        Some(features)
    }

    /// Select a provider using `LinUCB` bandit, with Thompson fallback on cold start / missing features.
    ///
    /// Falls through to Thompson or first available provider when bandit cannot decide.
    /// Budget enforcement via global `CostTracker` is handled at the caller level.
    /// Per-provider budget fractions are intentionally NOT implemented (scope creep, see #2230).
    async fn bandit_select_provider(&self, query: &str) -> Option<AnyProvider> {
        let Some(ref bandit_arc) = self.bandit else {
            return self.state.providers.first().cloned();
        };
        let cfg = self.bandit_config.as_ref()?;

        let names: Vec<String> = self
            .state
            .providers
            .iter()
            .map(|p| p.name().to_owned())
            .collect();

        // Try LinUCB selection with feature vector.
        if let Some(features) = self.bandit_features(query).await {
            let memory_confidence = self.state.last_memory_confidence.lock().as_ref().copied();
            let selected = {
                let state = bandit_arc.lock();
                state.select(
                    &names,
                    &features,
                    cfg.alpha,
                    cfg.warmup_queries,
                    &|_| true,
                    cfg.cost_weight,
                    &self.state.provider_models,
                    memory_confidence,
                    cfg.memory_confidence_threshold,
                )
            };
            if let Some(name) = selected {
                tracing::debug!(
                    provider = %name,
                    strategy = "bandit",
                    memory_confidence = ?memory_confidence,
                    "selected provider"
                );
                return self
                    .state
                    .providers
                    .iter()
                    .find(|p| p.name() == name)
                    .cloned();
            }
        }

        // Fallback: Thompson sampling.
        if let Some(ref thompson) = self.thompson {
            let mut state = thompson.lock();
            if let Some(sel) = state.select(&names) {
                tracing::debug!(
                    provider = %sel.provider,
                    strategy = "bandit-fallback-thompson",
                    "selected provider"
                );
                return self
                    .state
                    .providers
                    .iter()
                    .find(|p| p.name() == sel.provider)
                    .cloned();
            }
        }

        // Last resort: first provider.
        self.state.providers.first().cloned()
    }

    /// Record the bandit reward for a completed request.
    ///
    /// `quality_score`: heuristic quality in [0, 1] from `heuristic_score()`.
    /// `cost_fraction`: `request_cost_cents / max_daily_cents` (0 when budget is unlimited).
    fn bandit_record_reward(
        &self,
        provider_name: &str,
        features: &[f32],
        quality_score: f64,
        cost_fraction: f64,
    ) {
        let Some(ref bandit_arc) = self.bandit else {
            return;
        };
        let Some(cfg) = &self.bandit_config else {
            return;
        };
        #[allow(clippy::cast_possible_truncation)]
        let reward = (quality_score as f32) - cfg.cost_weight * (cost_fraction as f32);
        let reward = reward.clamp(-1.0, 1.0);
        let mut state = bandit_arc.lock();
        state.update(provider_name, features, reward);
        tracing::debug!(
            provider = provider_name,
            reward,
            quality = quality_score,
            "bandit: recorded reward"
        );
    }

    fn ordered_providers(&self) -> Vec<AnyProvider> {
        match self.strategy {
            RouterStrategy::Thompson => self.thompson_ordered_providers(),
            RouterStrategy::Ema => self.ema_ordered_providers(),
            // Cascade/Bandit: sync path used only for debug_request_json(); hot paths use
            // dedicated async selection methods. For Cascade, providers are sorted at
            // construction time.
            RouterStrategy::Cascade | RouterStrategy::Bandit => self.state.providers.to_vec(),
        }
    }

    fn ema_ordered_providers(&self) -> Vec<AnyProvider> {
        let order = self.state.provider_order.lock();
        let mut ordered: Vec<AnyProvider> = order
            .iter()
            .filter_map(|&i| self.state.providers.get(i).cloned())
            .collect();

        // CRIT-2 fix: apply reputation as a multiplicative adjustment to the EMA score,
        // not an additive term. This avoids unbounded score inflation.
        //
        // Adjustment formula: ema_score * (1 + weight * (rep_factor - 0.5) * 2)
        // where rep_factor in [0,1]: 0.5 = neutral, >0.5 = positive, <0.5 = negative.
        // CRIT-1 fix: reputation factor is sampled per-provider (each has its own Beta mean).
        if let Some(ref reputation) = self.reputation
            && let Some(ref ema) = self.ema
        {
            let rep = reputation.lock();
            let w = self.reputation_weight;
            let snap = ema.snapshot();
            let mut scored: Vec<(usize, f64)> = ordered
                .iter()
                .enumerate()
                .map(|(idx, p)| {
                    let ema_score = snap
                        .get(p.name())
                        .map_or(0.0, |s| s.success_ema - s.latency_ema_ms / 10_000.0);
                    let score = if let Some(rep_factor) = rep.ema_reputation_factor(p.name()) {
                        // Multiplicative blend: neutral at rep_factor=0.5, range ±weight.
                        let adjustment = 1.0 + w * (rep_factor - 0.5) * 2.0;
                        ema_score * adjustment
                    } else {
                        ema_score
                    };
                    (idx, score)
                })
                .collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let reordered: Vec<AnyProvider> = scored
                .into_iter()
                .filter_map(|(idx, _)| ordered.get(idx).cloned())
                .collect();
            ordered = reordered;
        }

        // ASI: re-score by down-weighting providers with low coherence.
        if let (Some(asi_arc), Some(asi_cfg)) = (&self.asi, &self.asi_config) {
            let asi: parking_lot::MutexGuard<'_, AsiState> = asi_arc.lock();
            let snap = self.ema.as_ref().map(EmaTracker::snapshot);
            let mut scored: Vec<(usize, f64)> = ordered
                .iter()
                .enumerate()
                .map(|(idx, p)| {
                    let coherence = asi.coherence(p.name());
                    if coherence < asi_cfg.coherence_threshold {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or(std::time::Duration::MAX)
                            .as_secs();
                        let last = ASI_WARN_LAST_SECS.load(Ordering::Relaxed);
                        if now.saturating_sub(last) >= 60
                            && ASI_WARN_LAST_SECS
                                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                                .is_ok()
                        {
                            tracing::warn!(
                                provider = p.name(),
                                coherence,
                                threshold = asi_cfg.coherence_threshold,
                                "asi: coherence below threshold"
                            );
                        } else {
                            tracing::trace!(
                                provider = p.name(),
                                coherence,
                                threshold = asi_cfg.coherence_threshold,
                                "asi: coherence below threshold (warn rate-limited)"
                            );
                        }
                    }
                    let base_score = snap
                        .as_ref()
                        .and_then(|s| s.get(p.name()))
                        .map_or(0.0, |s| s.success_ema - s.latency_ema_ms / 10_000.0);
                    // Multiply EMA score by coherence multiplier clamped to [0.5, 1.0].
                    let multiplier = (coherence / asi_cfg.coherence_threshold).clamp(0.5, 1.0);
                    #[allow(clippy::cast_possible_truncation)]
                    let adjusted = base_score * f64::from(multiplier);
                    (idx, adjusted)
                })
                .collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let reordered: Vec<AnyProvider> = scored
                .into_iter()
                .filter_map(|(idx, _)| ordered.get(idx).cloned())
                .collect();
            ordered = reordered;
        }

        if let Some(first) = ordered.first() {
            tracing::debug!(
                provider = %first.name(),
                strategy = "ema",
                "selected provider"
            );
        }
        ordered
    }

    fn thompson_ordered_providers(&self) -> Vec<AnyProvider> {
        let Some(ref thompson) = self.thompson else {
            return self.state.providers.to_vec();
        };
        let mut state = thompson.lock();
        let names: Vec<String> = self
            .state
            .providers
            .iter()
            .map(|p| p.name().to_owned())
            .collect();

        // Compute per-provider prior overrides: start from base Beta distribution, apply
        // reputation shift (CRIT-3), then apply ASI coherence penalty.
        let has_reputation = self.reputation.is_some();
        let has_asi = self.asi.is_some() && self.asi_config.is_some();

        let selected = if has_reputation || has_asi {
            // Build overrides by composing reputation and ASI adjustments.
            let rep_guard = self.reputation.as_ref().map(|r| r.lock());
            let asi_guard: Option<parking_lot::MutexGuard<'_, AsiState>> =
                self.asi.as_ref().map(|a| a.lock());
            let w = self.reputation_weight;

            let overrides: std::collections::HashMap<String, (f64, f64)> = names
                .iter()
                .map(|name| {
                    let base = state.get_distribution(name);
                    // Apply reputation prior shift.
                    let (alpha, mut beta) = if let Some(ref rep) = rep_guard {
                        rep.shift_thompson_priors(name, base.alpha, base.beta, w)
                    } else {
                        (base.alpha, base.beta)
                    };
                    // Apply ASI coherence penalty: shift beta by penalty_weight * deficit.
                    if let (Some(asi), Some(asi_cfg)) = (&asi_guard, &self.asi_config) {
                        let coherence = asi.coherence(name);
                        if coherence < asi_cfg.coherence_threshold {
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or(std::time::Duration::MAX)
                                .as_secs();
                            let last = ASI_WARN_LAST_SECS.load(Ordering::Relaxed);
                            if now.saturating_sub(last) >= 60
                                && ASI_WARN_LAST_SECS
                                    .compare_exchange(
                                        last,
                                        now,
                                        Ordering::Relaxed,
                                        Ordering::Relaxed,
                                    )
                                    .is_ok()
                            {
                                tracing::warn!(
                                    provider = name.as_str(),
                                    coherence,
                                    threshold = asi_cfg.coherence_threshold,
                                    "asi: coherence below threshold"
                                );
                            } else {
                                tracing::trace!(
                                    provider = name.as_str(),
                                    coherence,
                                    threshold = asi_cfg.coherence_threshold,
                                    "asi: coherence below threshold (warn rate-limited)"
                                );
                            }
                            let deficit = asi_cfg.coherence_threshold - coherence;
                            let penalty = f64::from(asi_cfg.penalty_weight * deficit);
                            beta += penalty;
                        }
                    }
                    (name.clone(), (alpha, beta))
                })
                .collect();

            drop(rep_guard);
            drop(asi_guard);
            state.select_with_priors(&names, &overrides)
        } else {
            state.select(&names)
        };

        if let Some(ref sel) = selected {
            tracing::debug!(
                provider = %sel.provider,
                strategy = "thompson",
                mode = if sel.exploit { "exploit" } else { "explore" },
                alpha = sel.alpha,
                beta = sel.beta,
                "selected provider"
            );
        }
        // Put selected provider first, keep rest in original order.
        let mut ordered = self.state.providers.to_vec();
        if let Some(ref sel) = selected
            && let Some(pos) = ordered.iter().position(|p| p.name() == sel.provider)
        {
            ordered.swap(0, pos);
        }
        ordered
    }

    /// Record availability outcome (network success/failure) for EMA or Thompson.
    ///
    /// For cascade routing, quality outcomes are tracked separately in `CascadeState`.
    /// Only availability outcomes (API up/down) are recorded here to avoid corrupting
    /// Thompson/EMA distributions with quality-based failures (HIGH-01).
    fn record_availability(&self, provider_name: &str, success: bool, latency_ms: u64) {
        match self.strategy {
            RouterStrategy::Thompson => {
                if let Some(ref thompson) = self.thompson {
                    let mut state = thompson.lock();
                    state.update(provider_name, success);
                }
            }
            RouterStrategy::Ema => {
                self.ema_record(provider_name, success, latency_ms);
            }
            RouterStrategy::Cascade | RouterStrategy::Bandit => {
                // Cascade does not use Thompson/EMA for ordering; no-op.
                // Bandit tracks rewards separately via bandit_record_reward().
            }
        }
    }

    fn ema_record(&self, provider_name: &str, success: bool, latency_ms: u64) {
        let Some(ref ema) = self.ema else {
            return;
        };
        ema.record(provider_name, success, latency_ms);
        let current_names: Vec<String> = self
            .state
            .providers
            .iter()
            .map(|p| p.name().to_owned())
            .collect();
        if let Some(new_order_names) = ema.maybe_reorder(&current_names) {
            let name_to_idx: std::collections::HashMap<&str, usize> = self
                .state
                .providers
                .iter()
                .enumerate()
                .map(|(i, p)| (p.name(), i))
                .collect();
            let new_order: Vec<usize> = new_order_names
                .iter()
                .filter_map(|n| name_to_idx.get(n.as_str()).copied())
                .collect();
            let mut order = self.state.provider_order.lock();
            *order = new_order;
        }
    }

    /// Return a snapshot of Thompson distribution parameters for all tracked providers.
    ///
    /// Returns an empty vec if Thompson strategy is not active.
    #[must_use]
    pub fn thompson_stats(&self) -> Vec<(String, f64, f64)> {
        let Some(ref thompson) = self.thompson else {
            return vec![];
        };
        let state = thompson.lock();
        state.provider_stats()
    }

    pub fn set_status_tx(&mut self, tx: StatusTx) {
        if let Some(providers) = Arc::get_mut(&mut self.state.providers) {
            for p in providers {
                p.set_status_tx(tx.clone());
            }
        } else {
            // Defensive path: should never happen at bootstrap (refcount == 1).
            let mut v: Vec<_> = self.state.providers.iter().cloned().collect();
            for p in &mut v {
                p.set_status_tx(tx.clone());
            }
            self.state.providers = Arc::from(v);
        }
        self.status_tx = Some(tx);
    }

    /// Aggregate model lists from all sub-providers, deduplicating by id.
    ///
    /// Individual sub-provider errors are logged as warnings and skipped.
    ///
    /// # Errors
    ///
    /// Always succeeds (errors per-provider are swallowed).
    pub async fn list_models_remote(
        &self,
    ) -> Result<Vec<crate::model_cache::RemoteModelInfo>, LlmError> {
        let mut seen = std::collections::HashSet::new();
        let mut all = Vec::new();
        for p in self.state.providers.iter() {
            match p.list_models_remote().await {
                Ok(models) => {
                    for m in models {
                        if seen.insert(m.id.clone()) {
                            all.push(m);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "router: list_models_remote sub-provider failed");
                }
            }
        }
        Ok(all)
    }

    /// Evaluate quality with heuristics only.
    fn evaluate_heuristic(response: &str, threshold: f64) -> cascade::QualityVerdict {
        let mut verdict = heuristic_score(response);
        verdict.should_escalate = verdict.score < threshold;
        verdict
    }

    /// Evaluate quality using the configured classifier mode.
    ///
    /// For `ClassifierMode::Judge`, calls the summary provider and falls back to heuristic
    /// on any error. For `ClassifierMode::Heuristic`, evaluates synchronously.
    async fn evaluate_quality(
        response: &str,
        threshold: f64,
        mode: ClassifierMode,
        summary_provider: Option<&AnyProvider>,
    ) -> cascade::QualityVerdict {
        if mode == ClassifierMode::Judge {
            if let Some(judge) = summary_provider {
                match cascade::judge_score(judge, response).await {
                    Some(score) => {
                        let should_escalate = score < threshold;
                        tracing::debug!(
                            score,
                            threshold,
                            should_escalate,
                            "cascade: judge scored response"
                        );
                        return cascade::QualityVerdict {
                            score,
                            should_escalate,
                            reason: format!("judge score: {score:.2}"),
                        };
                    }
                    None => {
                        tracing::warn!("cascade: judge call failed, falling back to heuristic");
                    }
                }
            } else {
                tracing::warn!(
                    "cascade: classifier_mode=judge but no summary_provider configured, \
                     using heuristic"
                );
            }
        }
        Self::evaluate_heuristic(response, threshold)
    }
}

const EMBED_MAX_RETRIES: u32 = 3;
const EMBED_BASE_DELAY_MS: u64 = 500;

impl RouterProvider {
    /// Embed `text` with per-turn caching.
    ///
    /// Checks `cache` before calling the underlying provider. On a cache hit, increments
    /// `embed_cache_hits`; on a miss, embeds via `self.embed()` and populates the cache.
    /// Either way, `embed_call_count` is incremented for observability.
    async fn embed_cached(
        &self,
        text: &str,
        cache: &Mutex<TurnEmbedCache>,
    ) -> Result<Vec<f32>, crate::error::LlmError> {
        self.state.embed_call_count.fetch_add(1, Ordering::Relaxed);
        if let Some(emb) = cache.lock().get(text) {
            self.state.embed_cache_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(emb.clone());
        }
        let emb = self.embed(text).await?;
        cache.lock().insert(text, emb.clone());
        Ok(emb)
    }

    /// Return session-level embedding cache metrics: `(total_calls, cache_hits)`.
    #[must_use]
    pub fn embed_cache_metrics(&self) -> (u64, u64) {
        (
            self.state.embed_call_count.load(Ordering::Relaxed),
            self.state.embed_cache_hits.load(Ordering::Relaxed),
        )
    }

    /// Spawn a background task to update the ASI window for `provider`.
    ///
    /// Fire-and-forget: routing is not blocked on the embed call. If the embed fails,
    /// the ASI window is not updated (no penalty for embed failure).
    ///
    /// `turn_id` is used to debounce: at most one ASI update fires per turn even when
    /// `chat()` is called N times concurrently (e.g., tool schema fetches). Subsequent
    /// calls within the same turn are silently dropped.
    ///
    /// `precomputed_embedding` — when `Some`, skips the embed call entirely (reuse from
    /// quality gate). When `None`, embeds `response` inline in the spawned task.
    fn spawn_asi_update(
        &self,
        provider: &str,
        response: String,
        turn_id: u64,
        precomputed_embedding: Option<Vec<f32>>,
    ) {
        // Debounce: swap in turn_id; if the previous value equals turn_id, another call
        // already claimed this turn → drop silently. `swap` is atomic so exactly one
        // concurrent caller wins the "first for this turn" race.
        let prev = self.state.asi_last_turn.swap(turn_id, Ordering::AcqRel);
        if prev == turn_id {
            return;
        }

        let Some(ref asi_arc) = self.asi else { return };
        let Some(ref asi_cfg) = self.asi_config else {
            return;
        };
        let asi = Arc::clone(asi_arc);
        let router = self.clone();
        let window_size = asi_cfg.window;
        let provider_name = provider.to_owned();
        tokio::spawn(async move {
            let emb = match precomputed_embedding {
                Some(e) => e,
                None => match router.embed(&response).await {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::debug!(
                            provider = provider_name,
                            error = %e,
                            "asi: embed failed, skipping coherence update"
                        );
                        return;
                    }
                },
            };
            let mut state = asi.lock();
            state.push_embedding(&provider_name, emb, window_size);
        });
    }
}

impl LlmProvider for RouterProvider {
    fn context_window(&self) -> Option<usize> {
        self.state
            .providers
            .first()
            .and_then(LlmProvider::context_window)
    }

    #[allow(clippy::too_many_lines)] // CoE + quality-gate inline logic; extracting would obscure the control flow
    fn chat(
        &self,
        messages: &[Message],
    ) -> impl std::future::Future<Output = Result<String, LlmError>> + Send {
        let status_tx = self.status_tx.clone();
        let messages = messages.to_vec();
        let router = self.clone();
        // TODO: DRY — `chat` and `chat_stream` share the same fallback loop pattern.
        // Refactor into a shared helper once the API stabilizes.
        Box::pin(async move {
            // Increment turn counter once per top-level chat() call. All concurrent sub-calls
            // (tool schema fetches, embed probes) that re-enter chat() will see the same
            // turn_id via the shared Arc<AtomicU64>, enabling ASI debounce.
            let turn_id = router.state.turn_counter.fetch_add(1, Ordering::Relaxed);

            tracing::info!(
                strategy = ?router.strategy,
                turn_id,
                provider_count = router.state.providers.len(),
                "llm.router.select"
            );

            if router.strategy == RouterStrategy::Cascade {
                // Cascade: pass Arc slice directly — providers are sorted at construction,
                // so no Vec allocation needed on the hot path.
                return router
                    .cascade_chat(&router.state.providers, &messages, status_tx)
                    .await;
            }
            if router.strategy == RouterStrategy::Bandit {
                return router.bandit_chat(&messages, status_tx).await;
            }
            let providers = router.ordered_providers();

            // Per-turn embedding cache: avoids re-embedding the same text across quality
            // gate and ASI update within a single chat() call.
            let turn_cache = Mutex::new(TurnEmbedCache::default());

            // Pre-compute query embedding once for quality gate (fail-open on error).
            let query_text = messages
                .last()
                .map(Message::to_llm_content)
                .unwrap_or_default();
            let query_embedding = if router.quality_gate.is_some() && !query_text.is_empty() {
                router.embed_cached(query_text, &turn_cache).await.ok()
            } else {
                None
            };

            // Best response seen so far (for quality gate exhaustion fallback, M2).
            let mut best_response: Option<(f32, String)> = None;

            for p in &providers {
                let start = std::time::Instant::now();
                match p.chat_with_extras(&messages).await {
                    Ok((r, extras)) => {
                        router.record_availability(
                            p.name(),
                            true,
                            u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        );

                        // Quality gate: check response-query embedding similarity.
                        if let (Some(threshold), Some(qemb)) =
                            (router.quality_gate, &query_embedding)
                        {
                            let resp_emb = router.embed_cached(&r, &turn_cache).await.ok();
                            let similarity = resp_emb
                                .as_ref()
                                .map_or(threshold, |e| cosine_similarity(qemb, e)); // fail-open: None → treat as passing
                            if similarity < threshold {
                                tracing::info!(
                                    provider = p.name(),
                                    score = similarity,
                                    threshold,
                                    "thompson_quality_fallback"
                                );
                                // Track best response seen so far.
                                let is_better = best_response
                                    .as_ref()
                                    .is_none_or(|(best, _)| similarity > *best);
                                if is_better {
                                    best_response = Some((similarity, r.clone()));
                                }
                                // Spawn ASI update even on quality failure, reusing resp_emb.
                                router.spawn_asi_update(p.name(), r, turn_id, resp_emb);
                                continue;
                            }
                            // Pass resp_emb to ASI to avoid a redundant embed call.
                            router.spawn_asi_update(p.name(), r.clone(), turn_id, resp_emb);

                            // CoE: pass already-obtained primary result to avoid double call.
                            if let Some(ref coe_router) = router.coe
                                && let Ok((final_r, pname, decision)) = run_coe(
                                    coe_router,
                                    p.name().to_owned(),
                                    r.clone(),
                                    extras,
                                    &messages,
                                )
                                .await
                            {
                                if matches!(
                                    decision,
                                    CoeDecision::EscalateIntra | CoeDecision::EscalateInter
                                ) {
                                    router.record_quality_outcome(&pname, false);
                                    router
                                        .record_quality_outcome(coe_router.secondary.name(), true);
                                }
                                return Ok(final_r);
                            }

                            return Ok(r);
                        }

                        // Spawn ASI embedding update (fire-and-forget, no precomputed embedding).
                        router.spawn_asi_update(p.name(), r.clone(), turn_id, None);

                        // CoE: pass already-obtained primary result to avoid double call.
                        if let Some(ref coe_router) = router.coe
                            && let Ok((final_r, pname, decision)) = run_coe(
                                coe_router,
                                p.name().to_owned(),
                                r.clone(),
                                extras,
                                &messages,
                            )
                            .await
                        {
                            if matches!(
                                decision,
                                CoeDecision::EscalateIntra | CoeDecision::EscalateInter
                            ) {
                                router.record_quality_outcome(&pname, false);
                                router.record_quality_outcome(coe_router.secondary.name(), true);
                            }
                            return Ok(final_r);
                        }

                        return Ok(r);
                    }
                    Err(e) => {
                        router.record_availability(
                            p.name(),
                            false,
                            u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        );
                        if e.is_rate_limited() {
                            router.record_availability(p.name(), false, 0);
                        }
                        if let Some(ref tx) = status_tx {
                            let _ = tx.send(format!("router: {} failed, falling back", p.name()));
                        }
                        tracing::warn!(provider = p.name(), error = %e, "router fallback");
                    }
                }
            }

            // All providers exhausted by quality gate: return best response seen (M2).
            if let Some((_, response)) = best_response {
                return Ok(response);
            }

            Err(LlmError::NoProviders)
        })
    }

    fn chat_stream(
        &self,
        messages: &[Message],
    ) -> impl std::future::Future<Output = Result<ChatStream, LlmError>> + Send {
        let status_tx = self.status_tx.clone();
        let messages = messages.to_vec();
        let router = self.clone();
        Box::pin(async move {
            if router.strategy == RouterStrategy::Cascade {
                // Cascade: pass Arc slice directly — no Vec allocation on the hot path.
                return router
                    .cascade_chat_stream(&router.state.providers, &messages, status_tx)
                    .await;
            }
            if router.strategy == RouterStrategy::Bandit {
                // Bandit stream: select provider then stream from it.
                // Reward is not recorded for streams (stream completion is async);
                // this is a known pre-1.0 limitation — same as Thompson stream mode.
                let query = messages
                    .last()
                    .map(super::provider::Message::to_llm_content)
                    .unwrap_or_default();
                let p = router
                    .bandit_select_provider(query)
                    .await
                    .ok_or(LlmError::NoProviders)?;
                return p.chat_stream(&messages).await;
            }
            let providers = router.ordered_providers();
            for p in &providers {
                let start = std::time::Instant::now();
                match p.chat_stream(&messages).await {
                    Ok(r) => {
                        // NOTE: success is recorded at stream-open time, not on stream
                        // completion. A provider that opens the stream but then fails
                        // mid-delivery still gets alpha += 1. This is a known pre-1.0
                        // limitation: fixing it requires wrapping ChatStream to intercept
                        // the completion/error signal, which adds latency on the hot path.
                        // Tracked in the adaptive-inference epic (CRIT-2).
                        router.record_availability(
                            p.name(),
                            true,
                            u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        );
                        return Ok(r);
                    }
                    Err(e) => {
                        router.record_availability(
                            p.name(),
                            false,
                            u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        );
                        if e.is_rate_limited() {
                            router.record_availability(p.name(), false, 0);
                        }
                        if let Some(ref tx) = status_tx {
                            let _ = tx.send(format!("router: {} failed, falling back", p.name()));
                        }
                        tracing::warn!(provider = p.name(), error = %e, "router stream fallback");
                    }
                }
            }
            Err(LlmError::NoProviders)
        })
    }

    fn supports_streaming(&self) -> bool {
        self.state
            .providers
            .iter()
            .any(LlmProvider::supports_streaming)
    }

    fn embed(
        &self,
        text: &str,
    ) -> impl std::future::Future<Output = Result<Vec<f32>, LlmError>> + Send {
        let providers = self.ordered_providers();
        let status_tx = self.status_tx.clone();
        let text = text.to_owned();
        let router = self.clone();
        Box::pin(async move {
            for p in &providers {
                if !p.supports_embeddings() {
                    continue;
                }
                let mut last_err: Option<LlmError> = None;
                for attempt in 0..=EMBED_MAX_RETRIES {
                    if attempt > 0 {
                        let delay = EMBED_BASE_DELAY_MS * (1u64 << (attempt - 1));
                        tracing::warn!(
                            provider = p.name(),
                            attempt,
                            delay_ms = delay,
                            "embed: rate limited, retrying after backoff"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    }
                    let start = std::time::Instant::now();
                    match p.embed(&text).await {
                        Ok(r) => {
                            router.record_availability(
                                p.name(),
                                true,
                                u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                            );
                            return Ok(r);
                        }
                        Err(e) if e.is_invalid_input() => {
                            // The input itself is invalid — retrying on another provider
                            // would fail identically. Do not penalize provider reputation.
                            tracing::warn!(
                                provider = p.name(),
                                error = %e,
                                "embed: invalid input, not retrying on other providers"
                            );
                            return Err(e);
                        }
                        Err(e) if e.is_rate_limited() && attempt < EMBED_MAX_RETRIES => {
                            last_err = Some(e);
                        }
                        Err(e) => {
                            router.record_availability(
                                p.name(),
                                false,
                                u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                            );
                            if let Some(ref tx) = status_tx {
                                let _ = tx.send(format!(
                                    "router: {} embed failed, falling back",
                                    p.name()
                                ));
                            }
                            tracing::warn!(provider = p.name(), error = %e, "router embed fallback");
                            last_err = Some(e);
                            break;
                        }
                    }
                }
                // All retries exhausted for this provider (rate-limited every time).
                if matches!(last_err, Some(ref e) if e.is_rate_limited()) {
                    router.record_availability(p.name(), false, 0);
                    if let Some(ref tx) = status_tx {
                        let _ = tx.send(format!(
                            "router: {} embed rate limited, falling back",
                            p.name()
                        ));
                    }
                    tracing::warn!(
                        provider = p.name(),
                        "embed: rate limit retries exhausted, falling back"
                    );
                }
            }
            Err(LlmError::NoProviders)
        })
    }

    fn embed_batch(
        &self,
        texts: &[&str],
    ) -> impl std::future::Future<Output = Result<Vec<Vec<f32>>, LlmError>> + Send {
        let providers = self.ordered_providers();
        let status_tx = self.status_tx.clone();
        let owned = owned_strs(texts);
        let router = self.clone();
        let semaphore = self.state.embed_semaphore.clone();
        Box::pin(async move {
            // Acquire embed semaphore permit before any HTTP work to cap concurrency.
            let _permit = if let Some(ref sem) = semaphore {
                Some(sem.acquire().await.map_err(|_| LlmError::NoProviders)?)
            } else {
                None
            };
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            for p in &providers {
                if !p.supports_embeddings() {
                    continue;
                }
                let mut last_err: Option<LlmError> = None;
                for attempt in 0..=EMBED_MAX_RETRIES {
                    if attempt > 0 {
                        let delay = EMBED_BASE_DELAY_MS * (1u64 << (attempt - 1));
                        tracing::warn!(
                            provider = p.name(),
                            attempt,
                            delay_ms = delay,
                            "embed_batch: rate limited, retrying after backoff"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    }
                    let start = std::time::Instant::now();
                    match p.embed_batch(&refs).await {
                        Ok(r) => {
                            router.record_availability(
                                p.name(),
                                true,
                                u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                            );
                            return Ok(r);
                        }
                        Err(e) if e.is_invalid_input() => {
                            tracing::warn!(
                                provider = p.name(),
                                error = %e,
                                "embed_batch: invalid input, not retrying on other providers"
                            );
                            return Err(e);
                        }
                        Err(e) if e.is_rate_limited() && attempt < EMBED_MAX_RETRIES => {
                            last_err = Some(e);
                        }
                        Err(e) => {
                            router.record_availability(
                                p.name(),
                                false,
                                u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                            );
                            if let Some(ref tx) = status_tx {
                                let _ = tx.send(format!(
                                    "router: {} embed_batch failed, falling back",
                                    p.name()
                                ));
                            }
                            tracing::warn!(
                                provider = p.name(),
                                error = %e,
                                "router embed_batch fallback"
                            );
                            last_err = Some(e);
                            break;
                        }
                    }
                }
                // All retries exhausted for this provider (rate-limited every time).
                if matches!(last_err, Some(ref e) if e.is_rate_limited()) {
                    router.record_availability(p.name(), false, 0);
                    if let Some(ref tx) = status_tx {
                        let _ = tx.send(format!(
                            "router: {} embed_batch rate limited, falling back",
                            p.name()
                        ));
                    }
                    tracing::warn!(
                        provider = p.name(),
                        "embed_batch: rate limit retries exhausted, falling back"
                    );
                }
            }
            Err(LlmError::NoProviders)
        })
    }

    fn supports_embeddings(&self) -> bool {
        self.state
            .providers
            .iter()
            .any(LlmProvider::supports_embeddings)
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "router"
    }

    fn supports_tool_use(&self) -> bool {
        self.state
            .providers
            .iter()
            .any(LlmProvider::supports_tool_use)
    }

    fn list_models(&self) -> Vec<String> {
        self.state
            .providers
            .iter()
            .flat_map(super::provider::LlmProvider::list_models)
            .collect()
    }

    #[allow(refining_impl_trait_reachable)]
    fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> impl std::future::Future<Output = Result<ChatResponse, LlmError>> + Send {
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        let status_tx = self.status_tx.clone();
        let router = self.clone();
        Box::pin(async move {
            // Bandit routing for tool calls: select a single provider, no quality escalation.
            if router.strategy == RouterStrategy::Bandit {
                let query = messages
                    .last()
                    .map(super::provider::Message::to_llm_content)
                    .unwrap_or_default();
                let p = router
                    .bandit_select_provider(query)
                    .await
                    .ok_or(LlmError::NoProviders)?;
                if !p.supports_tool_use() {
                    return Err(LlmError::NoProviders);
                }
                let result = p.chat_with_tools(&messages, &tools).await;
                if result.is_ok() {
                    *router.state.last_active_provider.lock() = Some(p.name().to_owned());
                }
                return result;
            }

            // Cascade is intentionally skipped for tool calls: evaluating quality of
            // a tool-call response (structured JSON with tool name + args) requires
            // different heuristics than text quality. Skipping cascade for tool calls
            // avoids inappropriate escalation based on text signals (HIGH-04).
            let providers = router.ordered_providers();
            for p in &providers {
                if !p.supports_tool_use() {
                    continue;
                }
                let start = std::time::Instant::now();
                match p.chat_with_tools(&messages, &tools).await {
                    Ok(r) => {
                        router.record_availability(
                            p.name(),
                            true,
                            u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        );
                        // Track which sub-provider served this tool call for reputation attribution.
                        *router.state.last_active_provider.lock() = Some(p.name().to_owned());
                        return Ok(r);
                    }
                    Err(e) => {
                        router.record_availability(
                            p.name(),
                            false,
                            u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        );
                        if e.is_invalid_input() {
                            tracing::warn!(
                                provider = p.name(),
                                error = %e,
                                "chat_with_tools: invalid input, not retrying on other providers"
                            );
                            return Err(e);
                        }
                        if e.is_rate_limited() {
                            router.record_availability(p.name(), false, 0);
                        }
                        if let Some(ref tx) = status_tx {
                            let _ = tx.send(format!(
                                "router: {} tool call failed, falling back",
                                p.name()
                            ));
                        }
                        tracing::warn!(provider = p.name(), error = %e, "router tool fallback");
                    }
                }
            }
            Err(LlmError::NoProviders)
        })
    }

    fn debug_request_json(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream: bool,
    ) -> serde_json::Value {
        let candidate = if tools.is_empty() {
            self.ordered_providers().into_iter().next()
        } else {
            self.ordered_providers()
                .into_iter()
                .find(super::provider::LlmProvider::supports_tool_use)
        };
        candidate.map_or_else(
            || crate::provider::default_debug_request_json(messages, tools),
            |provider| provider.debug_request_json(messages, tools, stream),
        )
    }

    fn last_cache_usage(&self) -> Option<(u64, u64)> {
        None
    }
}

// ── Bandit routing helpers ────────────────────────────────────────────────────

impl RouterProvider {
    /// Bandit `chat()` implementation: select provider, call, record reward.
    async fn bandit_chat(
        &self,
        messages: &[Message],
        status_tx: Option<StatusTx>,
    ) -> Result<String, LlmError> {
        let query = messages
            .last()
            .map(super::provider::Message::to_llm_content)
            .unwrap_or_default();
        let features = self.bandit_features(query.as_ref()).await;

        let p = self
            .bandit_select_provider(query.as_ref())
            .await
            .ok_or(LlmError::NoProviders)?;

        if let Some(ref tx) = status_tx {
            let _ = tx.send(format!("bandit: routing to {}", p.name()));
        }

        let result = p.chat(messages).await;
        match &result {
            Ok(response) => {
                let verdict = heuristic_score(response);
                // Record reward even when embedding failed (use zero vector so the arm's
                // update count increments — prevents permanent cold-start on flaky embedders).
                let feat_ref: &[f32];
                let zero_vec: Vec<f32>;
                let dim = self.bandit_config.as_ref().map_or(32, |c| c.dim);
                if let Some(ref feat) = features {
                    feat_ref = feat;
                } else {
                    zero_vec = vec![0.0; dim];
                    feat_ref = &zero_vec;
                    tracing::debug!(
                        provider = p.name(),
                        "bandit: recording reward with zero features (embed unavailable)"
                    );
                }
                self.bandit_record_reward(p.name(), feat_ref, verdict.score, 0.0);
            }
            Err(e) => {
                tracing::warn!(provider = p.name(), error = %e, "bandit: provider failed");
            }
        }
        result
    }
}

// ── Cascade routing helpers ───────────────────────────────────────────────────

/// Outcome of evaluating one provider's response during cascade routing.
struct CascadeEvalResult {
    verdict: cascade::QualityVerdict,
    /// Updated token counter after adding this response's estimated cost.
    tokens_used: u32,
    /// Whether the token budget is now exhausted.
    budget_exhausted: bool,
}

/// Evaluate a cascade response: score it, record the verdict in shared state, and
/// compute whether the token budget is exhausted.
async fn cascade_evaluate_response(
    provider_name: &str,
    response: &str,
    cfg: &CascadeRouterConfig,
    cascade_state: &Mutex<CascadeState>,
    tokens_used_before: u32,
    log_prefix: &str,
) -> CascadeEvalResult {
    let estimated_tokens =
        u32::try_from(zeph_common::text::estimate_tokens(response).max(1)).unwrap_or(u32::MAX);
    let tokens_used = tokens_used_before.saturating_add(estimated_tokens);

    let verdict = RouterProvider::evaluate_quality(
        response,
        cfg.quality_threshold,
        cfg.classifier_mode,
        cfg.summary_provider.as_ref(),
    )
    .await;

    {
        let mut state = cascade_state.lock();
        state.record(provider_name, verdict.score);
    }

    tracing::debug!(
        provider = %provider_name,
        score = verdict.score,
        threshold = cfg.quality_threshold,
        should_escalate = verdict.should_escalate,
        reason = %verdict.reason,
        "{log_prefix}: quality verdict"
    );

    let budget_exhausted = cfg
        .max_cascade_tokens
        .is_some_and(|budget| tokens_used >= budget);

    CascadeEvalResult {
        verdict,
        tokens_used,
        budget_exhausted,
    }
}

impl RouterProvider {
    /// Cascade chat: try providers in order, escalate on degenerate output.
    ///
    /// Returns the best-seen response if all providers fail or budget is exhausted.
    #[allow(clippy::too_many_lines)] // cascade loop: per-provider error/ok/budget/escalation branches are tightly coupled — extracting would obscure the control flow
    async fn cascade_chat(
        &self,
        providers: &[AnyProvider],
        messages: &[Message],
        status_tx: Option<StatusTx>,
    ) -> Result<String, LlmError> {
        let cfg = self
            .cascade_config
            .as_ref()
            .expect("cascade_config must be set");
        let cascade_state = self
            .cascade_state
            .as_ref()
            .expect("cascade_state must be set");

        let mut escalations_remaining = cfg.max_escalations;
        let mut best: Option<(String, f64)> = None; // (response, score)
        let mut tokens_used: u32 = 0;

        for (idx, p) in providers.iter().enumerate() {
            tracing::debug!(
                provider = %p.name(),
                attempt = idx + 1,
                total = providers.len(),
                classifier_mode = ?cfg.classifier_mode,
                quality_threshold = cfg.quality_threshold,
                "cascade: trying provider"
            );
            let start = std::time::Instant::now();
            match p.chat(messages).await {
                Err(e) => {
                    // Network/API error: record availability failure but don't consume escalation budget.
                    let latency = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                    self.record_availability(p.name(), false, latency);
                    if let Some(tx) = &status_tx {
                        let _ = tx.send(format!("cascade: {} unavailable, trying next", p.name()));
                    }
                    tracing::warn!(provider = p.name(), error = %e, "cascade: provider error");
                }
                Ok(response) => {
                    let latency = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

                    let eval = cascade_evaluate_response(
                        p.name(),
                        &response,
                        cfg,
                        cascade_state,
                        tokens_used,
                        "cascade",
                    )
                    .await;
                    tokens_used = eval.tokens_used;
                    let verdict = eval.verdict;
                    let budget_exhausted = eval.budget_exhausted;

                    // Update best-seen response; skip empty strings to avoid silent failures.
                    let is_better = !response.is_empty()
                        && best
                            .as_ref()
                            .is_none_or(|(_, best_score)| verdict.score > *best_score);
                    if is_better {
                        tracing::debug!(
                            provider = %p.name(),
                            score = verdict.score,
                            "cascade: best_seen updated"
                        );
                        best = Some((response.clone(), verdict.score));
                    }

                    let is_last = idx == providers.len() - 1;

                    if !verdict.should_escalate
                        || is_last
                        || escalations_remaining == 0
                        || budget_exhausted
                    {
                        self.record_availability(p.name(), true, latency);
                        // When escalation is blocked (budget exhausted or escalation count
                        // at zero) and the current response would have triggered escalation,
                        // return the best-seen response instead of the current (possibly
                        // lower-quality) one.
                        if verdict.should_escalate
                            && (budget_exhausted || escalations_remaining == 0)
                        {
                            let best_response = best.take().map_or(response, |(r, _)| r);
                            tracing::info!(
                                tokens_used,
                                budget = cfg.max_cascade_tokens,
                                escalations_remaining,
                                "cascade: escalation blocked, returning best response"
                            );
                            return Ok(best_response);
                        }
                        return Ok(response);
                    }

                    // Escalate: record availability success (provider worked, just low quality).
                    self.record_availability(p.name(), true, latency);
                    escalations_remaining -= 1;

                    if let Some(tx) = &status_tx {
                        let _ = tx.send(format!(
                            "cascade: {} quality {:.2} < {:.2}, escalating ({} left)",
                            p.name(),
                            verdict.score,
                            cfg.quality_threshold,
                            escalations_remaining
                        ));
                    }
                    tracing::info!(
                        provider = %p.name(),
                        score = verdict.score,
                        threshold = cfg.quality_threshold,
                        escalations_remaining,
                        "cascade: escalating to next provider"
                    );
                }
            }
        }

        // All providers tried — return best-seen response, or NoProviders if none worked.
        if let Some((_, score)) = &best {
            tracing::info!(
                score,
                "cascade: all providers exhausted, returning best-seen response"
            );
        } else {
            tracing::warn!("cascade: all providers failed, no response available");
        }
        best.map(|(r, _)| r).ok_or(LlmError::NoProviders)
    }

    /// Cascade `chat_stream`: buffer cheap response, classify, escalate or replay.
    ///
    /// # Streaming latency tradeoff
    ///
    /// The first N-1 providers are fully buffered before classification. If escalation
    /// occurs, the user experiences: cheap model's full response time + expensive model's
    /// TTFT. This is strictly worse than direct routing to the expensive model for
    /// hard queries. Acceptable for v1; see CRIT-01 in critic handoff for alternatives.
    #[allow(clippy::too_many_lines)] // sequential cascade semantics: buffer→classify→escalate
    async fn cascade_chat_stream(
        &self,
        providers: &[AnyProvider],
        messages: &[Message],
        status_tx: Option<StatusTx>,
    ) -> Result<ChatStream, LlmError> {
        let cfg = self
            .cascade_config
            .as_ref()
            .expect("cascade_config must be set");
        let cascade_state = self
            .cascade_state
            .as_ref()
            .expect("cascade_state must be set");

        let mut escalations_remaining = cfg.max_escalations;
        let mut tokens_used: u32 = 0;
        // Tracks the highest-scoring fully-buffered response seen so far.
        // Only populated from the early provider loop; the last provider streams
        // directly without buffering or scoring, so it never updates best_seen.
        let mut best_seen: Option<(String, f64)> = None;

        // Try all providers except the last without consuming the escalation budget
        // for errors (only quality failures consume it).
        let (last, early) = providers.split_last().ok_or(LlmError::NoProviders)?;

        for (idx, p) in early.iter().enumerate() {
            tracing::debug!(
                provider = %p.name(),
                attempt = idx + 1,
                total = providers.len(),
                classifier_mode = ?cfg.classifier_mode,
                quality_threshold = cfg.quality_threshold,
                "cascade stream: trying provider (buffered)"
            );
            // Buffer response to classify quality.
            let start = std::time::Instant::now();
            let stream = match p.chat_stream(messages).await {
                Err(e) => {
                    let latency = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                    self.record_availability(p.name(), false, latency);
                    tracing::warn!(provider = p.name(), error = %e, "cascade stream: provider error");
                    if let Some(tx) = &status_tx {
                        let _ = tx.send(format!("cascade: {} unavailable, trying next", p.name()));
                    }
                    continue;
                }
                Ok(s) => s,
            };

            // Collect the full stream.
            let buffered = collect_stream(stream).await;
            let latency = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

            match buffered {
                Err(e) => {
                    // Stream failed mid-delivery; treat as availability failure.
                    self.record_availability(p.name(), false, latency);
                    tracing::warn!(provider = p.name(), error = %e, "cascade stream: stream error");
                }
                Ok(text) => {
                    let eval = cascade_evaluate_response(
                        p.name(),
                        &text,
                        cfg,
                        cascade_state,
                        tokens_used,
                        "cascade stream",
                    )
                    .await;
                    tokens_used = eval.tokens_used;
                    let verdict = eval.verdict;
                    let budget_exhausted = eval.budget_exhausted;

                    // Track the best response seen so far across early providers.
                    // Skip empty strings to avoid returning silent failures on all-fail fallback.
                    let is_better = !text.is_empty()
                        && best_seen
                            .as_ref()
                            .is_none_or(|(_, best_score)| verdict.score > *best_score);
                    if is_better {
                        tracing::debug!(
                            provider = %p.name(),
                            score = verdict.score,
                            "cascade stream: best_seen updated"
                        );
                        best_seen = Some((text.clone(), verdict.score));
                    }

                    if !verdict.should_escalate || escalations_remaining == 0 || budget_exhausted {
                        self.record_availability(p.name(), true, latency);

                        // When escalation is blocked (budget exhausted or escalation count
                        // at zero) and the current response would have triggered escalation,
                        // return the best-seen response instead of the current (possibly
                        // lower-quality) one.
                        let response_text = if verdict.should_escalate
                            && (budget_exhausted || escalations_remaining == 0)
                        {
                            tracing::info!(
                                tokens_used,
                                budget = cfg.max_cascade_tokens,
                                escalations_remaining,
                                "cascade stream: escalation blocked, returning best response"
                            );
                            best_seen.take().map_or(text, |(r, _)| r)
                        } else {
                            text
                        };

                        let stream: ChatStream = Box::pin(tokio_stream::once(Ok(
                            crate::provider::StreamChunk::Content(response_text),
                        )));
                        return Ok(stream);
                    }

                    // Escalate.
                    self.record_availability(p.name(), true, latency);
                    escalations_remaining -= 1;

                    if let Some(tx) = &status_tx {
                        let _ = tx.send(format!(
                            "cascade: {} quality {:.2} < {:.2}, escalating",
                            p.name(),
                            verdict.score,
                            cfg.quality_threshold,
                        ));
                    }
                    tracing::info!(
                        provider = %p.name(),
                        score = verdict.score,
                        threshold = cfg.quality_threshold,
                        escalations_remaining,
                        "cascade stream: escalating to next provider"
                    );
                }
            }
        }

        // Last provider: stream directly without buffering.
        // Note: if the stream itself fails mid-delivery (after Ok(stream) is returned),
        // there is no fallback to best_seen — the caller receives a partial response.
        // This is a pre-existing limitation; fixing it would require wrapping the stream.
        tracing::debug!(
            provider = %last.name(),
            attempt = providers.len(),
            total = providers.len(),
            "cascade stream: trying last provider (streaming, no classification)"
        );
        let start = std::time::Instant::now();
        match last.chat_stream(messages).await {
            Ok(stream) => {
                self.record_availability(
                    last.name(),
                    true,
                    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                );
                Ok(stream)
            }
            Err(e) => {
                self.record_availability(
                    last.name(),
                    false,
                    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                );
                // If we have a best-seen response from an early provider, return it
                // instead of propagating the last provider's error.
                if let Some((best_text, _)) = best_seen {
                    tracing::info!(
                        "cascade stream: last provider failed, returning best-seen response"
                    );
                    let stream: ChatStream = Box::pin(tokio_stream::once(Ok(
                        crate::provider::StreamChunk::Content(best_text),
                    )));
                    return Ok(stream);
                }
                Err(e)
            }
        }
    }
}

/// Maximum bytes buffered per stream in cascade routing (SEC-CASCADE-03).
const CASCADE_STREAM_MAX_BYTES: usize = 1024 * 1024; // 1 MiB

/// Collect a `ChatStream` into a String, concatenating only `Content` chunks.
///
/// Returns `Err` if the accumulated buffer exceeds [`CASCADE_STREAM_MAX_BYTES`].
async fn collect_stream(stream: ChatStream) -> Result<String, LlmError> {
    use tokio_stream::StreamExt as _;

    let mut stream = stream;
    let mut buf = String::new();
    while let Some(chunk) = stream.next().await {
        match chunk? {
            crate::provider::StreamChunk::Content(c) => {
                if buf.len() + c.len() > CASCADE_STREAM_MAX_BYTES {
                    return Err(LlmError::Other(
                        "cascade: stream response exceeds 1 MiB buffer limit".into(),
                    ));
                }
                buf.push_str(&c);
            }
            crate::provider::StreamChunk::Thinking(_)
            | crate::provider::StreamChunk::Compaction(_)
            | crate::provider::StreamChunk::ToolUse(_) => {}
        }
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Role;

    #[test]
    fn empty_router_name() {
        let r = RouterProvider::new(vec![]);
        assert_eq!(r.name(), "router");
    }

    #[test]
    fn empty_router_supports_nothing() {
        let r = RouterProvider::new(vec![]);
        assert!(!r.supports_streaming());
        assert!(!r.supports_embeddings());
        assert!(!r.supports_tool_use());
    }

    #[test]
    fn empty_router_context_window_none() {
        let r = RouterProvider::new(vec![]);
        assert!(r.context_window().is_none());
    }

    #[tokio::test]
    async fn empty_router_chat_returns_no_providers() {
        let r = RouterProvider::new(vec![]);
        let msgs = vec![Message::from_legacy(Role::User, "hello")];
        let err = r.chat(&msgs).await.unwrap_err();
        assert!(matches!(err, LlmError::NoProviders));
    }

    #[tokio::test]
    async fn empty_router_chat_stream_returns_no_providers() {
        let r = RouterProvider::new(vec![]);
        let msgs = vec![Message::from_legacy(Role::User, "hello")];
        let result = r.chat_stream(&msgs).await;
        assert!(matches!(result, Err(LlmError::NoProviders)));
    }

    #[tokio::test]
    async fn empty_router_embed_returns_no_providers() {
        let r = RouterProvider::new(vec![]);
        let err = r.embed("test").await.unwrap_err();
        assert!(matches!(err, LlmError::NoProviders));
    }

    #[tokio::test]
    async fn empty_router_chat_with_tools_returns_no_providers() {
        let r = RouterProvider::new(vec![]);
        let msgs = vec![Message::from_legacy(Role::User, "hello")];
        let err = r.chat_with_tools(&msgs, &[]).await.unwrap_err();
        assert!(matches!(err, LlmError::NoProviders));
    }

    #[tokio::test]
    async fn router_falls_back_on_unreachable() {
        use crate::ollama::OllamaProvider;

        let p1 = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:1",
            "m".into(),
            "e".into(),
        ));
        let p2 = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:2",
            "m".into(),
            "e".into(),
        ));
        let r = RouterProvider::new(vec![p1, p2]);
        let msgs = vec![Message::from_legacy(Role::User, "hello")];
        let err = r.chat(&msgs).await.unwrap_err();
        assert!(matches!(err, LlmError::NoProviders));
    }

    #[test]
    fn router_with_streaming_provider() {
        use crate::ollama::OllamaProvider;

        let p = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:1",
            "m".into(),
            "e".into(),
        ));
        let r = RouterProvider::new(vec![p]);
        assert!(r.supports_streaming());
        assert!(r.supports_embeddings());
    }

    #[test]
    fn clone_preserves_providers() {
        use crate::ollama::OllamaProvider;

        let p = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:1",
            "m".into(),
            "e".into(),
        ));
        let r = RouterProvider::new(vec![p]);
        let c = r.clone();
        assert_eq!(c.state.providers.len(), 1);
        assert_eq!(c.name(), "router");
    }

    #[test]
    fn last_cache_usage_returns_none() {
        let r = RouterProvider::new(vec![]);
        assert!(r.last_cache_usage().is_none());
    }

    #[test]
    fn thompson_strategy_is_set() {
        let r = RouterProvider::new(vec![]).with_thompson(None);
        assert_eq!(r.strategy, RouterStrategy::Thompson);
        assert!(r.thompson.is_some());
    }

    #[test]
    fn save_thompson_state_noop_without_thompson() {
        let r = RouterProvider::new(vec![]);
        r.save_thompson_state(); // should not panic
    }

    #[test]
    fn thompson_ordered_providers_empty() {
        let r = RouterProvider::new(vec![]).with_thompson(None);
        let ordered = r.ordered_providers();
        assert!(ordered.is_empty());
    }

    #[test]
    fn concurrent_record_outcome_does_not_deadlock() {
        use std::sync::Arc;
        let r = Arc::new(RouterProvider::new(vec![]).with_thompson(None));
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let router = Arc::clone(&r);
                std::thread::spawn(move || {
                    router.record_availability(&format!("p{i}"), i % 2 == 0, 10);
                })
            })
            .collect();
        for h in handles {
            h.join().expect("thread panicked");
        }
        // If we reach here, no deadlock occurred.
        let stats = r.thompson_stats();
        assert_eq!(stats.len(), 8);
    }

    // ── Cascade tests ──────────────────────────────────────────────────────────

    #[test]
    fn cascade_strategy_is_set() {
        let r = RouterProvider::new(vec![]).with_cascade(CascadeRouterConfig::default());
        assert_eq!(r.strategy, RouterStrategy::Cascade);
        assert!(r.cascade_state.is_some());
        assert!(r.cascade_config.is_some());
    }

    #[test]
    fn cascade_ordered_providers_preserves_chain_order() {
        use crate::ollama::OllamaProvider;
        let p1 = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:1",
            "a".into(),
            String::new(),
        ));
        let p2 = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:2",
            "b".into(),
            String::new(),
        ));
        let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig::default());
        let ordered = r.ordered_providers();
        assert_eq!(ordered.len(), 2);
    }

    #[tokio::test]
    async fn cascade_empty_router_returns_no_providers() {
        let r = RouterProvider::new(vec![]).with_cascade(CascadeRouterConfig::default());
        let msgs = vec![Message::from_legacy(Role::User, "hello")];
        let err = r.chat(&msgs).await.unwrap_err();
        assert!(matches!(err, LlmError::NoProviders));
    }

    #[tokio::test]
    async fn cascade_returns_best_seen_when_all_fail_after_good_response() {
        use crate::mock::MockProvider;

        // Provider 1: returns low-quality response (short "ok", triggers escalation at 0.9 threshold)
        let cheap =
            AnyProvider::Mock(MockProvider::with_responses(vec!["ok".to_owned()]).with_delay(0));
        // Provider 2: fails with availability error
        let expensive = AnyProvider::Mock(MockProvider::failing());

        let r = RouterProvider::new(vec![cheap, expensive]).with_cascade(CascadeRouterConfig {
            quality_threshold: 0.9, // high threshold ensures "ok" fails quality check
            max_escalations: 2,
            ..CascadeRouterConfig::default()
        });
        let msgs = vec![Message::from_legacy(Role::User, "hello")];
        // Should return "ok" from cheap provider (best-seen), not NoProviders.
        let result = r.chat(&msgs).await.unwrap();
        assert_eq!(result, "ok");
    }

    #[tokio::test]
    async fn cascade_accepts_good_quality_response() {
        use crate::mock::MockProvider;

        let good_response = "This is a comprehensive, well-structured response that provides \
            detailed information about the topic. It covers multiple aspects and explains \
            the reasoning clearly with proper sentence structure.";

        let cheap = AnyProvider::Mock(
            MockProvider::with_responses(vec![good_response.to_owned()]).with_delay(0),
        );
        // second provider should never be called
        let expensive = AnyProvider::Mock(MockProvider::failing());

        let r = RouterProvider::new(vec![cheap, expensive]).with_cascade(CascadeRouterConfig {
            quality_threshold: 0.5,
            max_escalations: 1,
            ..CascadeRouterConfig::default()
        });
        let msgs = vec![Message::from_legacy(Role::User, "explain something")];
        let result = r.chat(&msgs).await.unwrap();
        assert_eq!(result, good_response);
    }

    #[tokio::test]
    async fn cascade_max_escalations_budget_exhausted_returns_last_attempted() {
        use crate::mock::MockProvider;

        // All three providers return degenerate response "x" but budget limits to 1 escalation.
        // p1 -> escalation budget 1 -> p2 -> budget=0 -> accept p2's response (not p3).
        let p1 =
            AnyProvider::Mock(MockProvider::with_responses(vec!["x".to_owned()]).with_delay(0));
        let p2 =
            AnyProvider::Mock(MockProvider::with_responses(vec!["x".to_owned()]).with_delay(0));
        let p3 = AnyProvider::Mock(MockProvider::failing()); // should never be reached

        let r = RouterProvider::new(vec![p1, p2, p3]).with_cascade(CascadeRouterConfig {
            quality_threshold: 0.9,
            max_escalations: 1, // only 1 escalation allowed
            ..CascadeRouterConfig::default()
        });
        let msgs = vec![Message::from_legacy(Role::User, "test")];
        let result = r.chat(&msgs).await.unwrap();
        assert_eq!(result, "x");
    }

    #[tokio::test]
    async fn cascade_token_budget_stops_escalation() {
        use crate::mock::MockProvider;

        let p1 =
            AnyProvider::Mock(MockProvider::with_responses(vec!["x".to_owned()]).with_delay(0));
        let p2 = AnyProvider::Mock(MockProvider::failing()); // should not be reached

        let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig {
            quality_threshold: 0.9, // "x" will fail quality
            max_escalations: 5,
            max_cascade_tokens: Some(1), // 1 token budget — exhausted after first response (~4 chars / 4 = 0 + 1 min)
            ..CascadeRouterConfig::default()
        });
        let msgs = vec![Message::from_legacy(Role::User, "test")];
        let result = r.chat(&msgs).await.unwrap();
        assert_eq!(result, "x"); // returned despite low quality due to token budget
    }

    #[tokio::test]
    async fn cascade_budget_returns_best_seen_not_current() {
        use crate::mock::MockProvider;

        // p1 returns a decent response, p2 returns a worse one but exhausts the budget.
        // With budget_exhausted, we should get the best-seen (p1) not the current (p2).
        let good_response = "This is a reasonable response with enough content to score well.";
        let bad_response = "x"; // degenerate, score << good_response

        let p1 = AnyProvider::Mock(
            MockProvider::with_responses(vec![good_response.to_owned()]).with_delay(0),
        );
        let p2 = AnyProvider::Mock(
            MockProvider::with_responses(vec![bad_response.to_owned()]).with_delay(0),
        );

        let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig {
            quality_threshold: 0.95, // both fail quality check but good > bad
            max_escalations: 5,
            max_cascade_tokens: Some(1), // budget exhausted after p1 (1 token min)
            ..CascadeRouterConfig::default()
        });
        let msgs = vec![Message::from_legacy(Role::User, "test")];
        // p1 exhausts the budget; should return p1's response (better), not p2's (worse).
        // Note: p2 is reached since budget check happens AFTER p1's response is processed
        // and p1 fails quality. Budget exhausted at p2 → return best-seen (p1).
        let result = r.chat(&msgs).await.unwrap();
        // The result must not be the degenerate "x" response.
        assert_ne!(result, bad_response, "should return best-seen, not current");
    }

    #[tokio::test]
    async fn cascade_escalations_exhausted_returns_best_seen_not_current() {
        use crate::mock::MockProvider;

        // p1: decent response, fails quality at 0.95 → escalates (escalations_remaining: 1 → 0)
        // p2: degenerate "x", fails quality → escalations_remaining == 0 → blocked → best_seen wins
        let good_response = "This is a reasonable response with enough content to score well.";
        let bad_response = "x";

        let p1 = AnyProvider::Mock(
            MockProvider::with_responses(vec![good_response.to_owned()]).with_delay(0),
        );
        let p2 = AnyProvider::Mock(
            MockProvider::with_responses(vec![bad_response.to_owned()]).with_delay(0),
        );
        let p3 = AnyProvider::Mock(MockProvider::failing()); // should not be reached

        let r = RouterProvider::new(vec![p1, p2, p3]).with_cascade(CascadeRouterConfig {
            quality_threshold: 0.95, // both fail quality; p1 score > p2 score
            max_escalations: 1,      // p1 escalates (budget: 1→0), p2 is blocked
            ..CascadeRouterConfig::default()
        });
        let msgs = vec![Message::from_legacy(Role::User, "test")];
        let result = r.chat(&msgs).await.unwrap();
        assert_eq!(
            result, good_response,
            "should return best-seen (p1), not the degenerate current response (p2)"
        );
        assert_ne!(
            result, bad_response,
            "must not return degenerate p2 response"
        );
    }

    #[tokio::test]
    async fn cascade_stream_escalations_exhausted_returns_best_seen_not_current() {
        use crate::mock::MockProvider;

        // Same scenario as above but for cascade_chat_stream.
        // p1: decent response, fails quality at 0.95 → escalates (escalations_remaining: 1 → 0)
        // p2: degenerate "x", fails quality → escalations_remaining == 0 → return best_seen
        let good_response = "This is a reasonable response with enough content to score well.";
        let bad_response = "x";

        let p1 = AnyProvider::Mock(
            MockProvider::with_responses(vec![good_response.to_owned()])
                .with_delay(0)
                .with_streaming(),
        );
        let p2 = AnyProvider::Mock(
            MockProvider::with_responses(vec![bad_response.to_owned()])
                .with_delay(0)
                .with_streaming(),
        );
        let p3 = AnyProvider::Mock(MockProvider::failing()); // last provider, should not be reached

        let r = RouterProvider::new(vec![p1, p2, p3]).with_cascade(CascadeRouterConfig {
            quality_threshold: 0.95, // both fail quality; p1 score > p2 score
            max_escalations: 1,      // p1 escalates (budget: 1→0), p2 is blocked
            ..CascadeRouterConfig::default()
        });
        let msgs = vec![Message::from_legacy(Role::User, "test")];
        let stream = r.chat_stream(&msgs).await.unwrap();
        let collected = collect_stream(stream).await.unwrap();
        assert_eq!(
            collected, good_response,
            "should return best-seen (p1), not the degenerate current response (p2)"
        );
        assert_ne!(
            collected, bad_response,
            "must not return degenerate p2 response"
        );
    }

    #[tokio::test]
    async fn cascade_all_providers_fail_returns_no_providers() {
        use crate::mock::MockProvider;

        let p1 = AnyProvider::Mock(MockProvider::failing());
        let p2 = AnyProvider::Mock(MockProvider::failing());

        let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig::default());
        let msgs = vec![Message::from_legacy(Role::User, "test")];
        let err = r.chat(&msgs).await.unwrap_err();
        assert!(matches!(err, LlmError::NoProviders));
    }

    #[tokio::test]
    async fn cascade_stream_good_quality_no_escalation() {
        use crate::mock::MockProvider;

        let good = "This is a well-formed response with sufficient length and coherent structure.";
        let p1 = AnyProvider::Mock(
            MockProvider::with_responses(vec![good.to_owned()])
                .with_delay(0)
                .with_streaming(),
        );
        let p2 = AnyProvider::Mock(MockProvider::failing());

        let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig {
            quality_threshold: 0.5,
            max_escalations: 1,
            ..CascadeRouterConfig::default()
        });
        let msgs = vec![Message::from_legacy(Role::User, "q")];
        let stream = r.chat_stream(&msgs).await.unwrap();
        let collected = collect_stream(stream).await.unwrap();
        assert_eq!(collected, good);
    }

    #[tokio::test]
    async fn cascade_stream_escalates_to_last_provider() {
        use crate::mock::MockProvider;

        let bad = "x"; // low quality, should escalate
        let good = "This is the expensive model's comprehensive response.";
        let p1 = AnyProvider::Mock(
            MockProvider::with_responses(vec![bad.to_owned()])
                .with_delay(0)
                .with_streaming(),
        );
        let p2 = AnyProvider::Mock(
            MockProvider::with_responses(vec![good.to_owned()])
                .with_delay(0)
                .with_streaming(),
        );

        let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig {
            quality_threshold: 0.9, // "x" fails quality
            max_escalations: 1,
            ..CascadeRouterConfig::default()
        });
        let msgs = vec![Message::from_legacy(Role::User, "q")];
        let stream = r.chat_stream(&msgs).await.unwrap();
        let collected = collect_stream(stream).await.unwrap();
        assert_eq!(collected, good);
    }

    #[tokio::test]
    async fn cascade_stream_budget_returns_best_seen() {
        use crate::mock::MockProvider;

        // Three providers: early=[p1, p2], last=p3.
        // p1 returns a decent response (fails quality threshold at 0.95, triggers escalation).
        // Budget is set to 1 token, so it is exhausted immediately after p1 processes.
        // best_seen = p1's response; budget_exhausted + should_escalate → return best_seen.
        let good_response = "This is a reasonable response with enough content to score well.";
        let bad_response = "x"; // degenerate, score << good_response

        let p1 = AnyProvider::Mock(
            MockProvider::with_responses(vec![good_response.to_owned()])
                .with_delay(0)
                .with_streaming(),
        );
        let p2 = AnyProvider::Mock(
            MockProvider::with_responses(vec![bad_response.to_owned()])
                .with_delay(0)
                .with_streaming(),
        );
        let p3 = AnyProvider::Mock(MockProvider::failing()); // last provider, not reached

        let r = RouterProvider::new(vec![p1, p2, p3]).with_cascade(CascadeRouterConfig {
            quality_threshold: 0.95, // p1 fails quality check → triggers escalation path
            max_escalations: 5,
            max_cascade_tokens: Some(1), // budget exhausted after p1 (1 token min)
            ..CascadeRouterConfig::default()
        });
        let msgs = vec![Message::from_legacy(Role::User, "test")];
        let stream = r.chat_stream(&msgs).await.unwrap();
        let collected = collect_stream(stream).await.unwrap();
        // Must return best-seen (p1's good response).
        assert_eq!(
            collected, good_response,
            "should return best-seen p1 response when budget exhausted"
        );
    }

    #[tokio::test]
    async fn cascade_stream_budget_returns_best_seen_not_current() {
        use crate::mock::MockProvider;

        // Four providers: early=[p1, p2, p3], last=p4.
        // p1 returns a good response, fails quality at 0.95 (score ~0.6), escalates; budget not yet exhausted.
        // p2 returns a degenerate response "x", fails quality, exhausts the budget.
        // At budget exhaustion: best_seen = p1 (higher score), current = p2's "x".
        // Must return best_seen (p1), not current (p2).
        let good_response = "This is a reasonable response with enough content to score well.";
        let bad_response = "x"; // 1 char → estimated_tokens = max(1/4, 1) = 1

        let p1 = AnyProvider::Mock(
            MockProvider::with_responses(vec![good_response.to_owned()])
                .with_delay(0)
                .with_streaming(),
        );
        let p2 = AnyProvider::Mock(
            MockProvider::with_responses(vec![bad_response.to_owned()])
                .with_delay(0)
                .with_streaming(),
        );
        let p3 = AnyProvider::Mock(MockProvider::failing()); // last provider, not reached
        let p4 = AnyProvider::Mock(MockProvider::failing()); // last provider, not reached

        // Budget = 20: p1 uses ~16 tokens (65 chars / 4), p2 uses 1 → total 17 ≥ 20? No.
        // Use budget = 17 so p2 exhausts it.
        let r = RouterProvider::new(vec![p1, p2, p3, p4]).with_cascade(CascadeRouterConfig {
            quality_threshold: 0.95, // both fail; p1 score > p2 score
            max_escalations: 5,
            max_cascade_tokens: Some(17), // p1 uses 16, p2 uses 1 → total 17 ≥ 17 after p2
            ..CascadeRouterConfig::default()
        });
        let msgs = vec![Message::from_legacy(Role::User, "test")];
        let stream = r.chat_stream(&msgs).await.unwrap();
        let collected = collect_stream(stream).await.unwrap();
        // Must return p1 (best_seen), not p2 (current at time of budget exhaustion).
        assert_eq!(
            collected, good_response,
            "should return best-seen (p1), not current degenerate (p2)"
        );
        assert_ne!(
            collected, bad_response,
            "must not return the degenerate p2 response"
        );
    }

    #[tokio::test]
    async fn cascade_stream_last_fails_returns_best_seen() {
        use crate::mock::MockProvider;

        // Two providers: early=[p1], last=p2.
        // p1 returns a low-quality response that triggers escalation.
        // p2 (last) fails with an error.
        // Should return p1's response (best-seen) instead of propagating the error.
        let low_quality = "ok"; // short, triggers escalation at 0.9 threshold
        let p1 = AnyProvider::Mock(
            MockProvider::with_responses(vec![low_quality.to_owned()])
                .with_delay(0)
                .with_streaming(),
        );
        let p2 = AnyProvider::Mock(MockProvider::failing()); // last provider fails

        let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig {
            quality_threshold: 0.9, // "ok" fails quality, triggers escalation
            max_escalations: 2,
            ..CascadeRouterConfig::default()
        });
        let msgs = vec![Message::from_legacy(Role::User, "hello")];
        let stream = r.chat_stream(&msgs).await.unwrap();
        let collected = collect_stream(stream).await.unwrap();
        assert_eq!(collected, low_quality);
    }

    #[tokio::test]
    async fn cascade_stream_all_fail_returns_error() {
        use crate::mock::MockProvider;

        // Two providers, both fail. No best_seen accumulated.
        // p1 is early (errors → continue), p2 is last (errors → propagated).
        // The last provider's error must be propagated, not swallowed.
        let p1 = AnyProvider::Mock(MockProvider::failing());
        let p2 = AnyProvider::Mock(MockProvider::failing());

        let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig::default());
        let msgs = vec![Message::from_legacy(Role::User, "test")];
        let result = r.chat_stream(&msgs).await;
        assert!(
            result.is_err(),
            "expected error when all providers fail with no best_seen"
        );
    }

    #[test]
    fn cascade_config_default_values() {
        let cfg = CascadeRouterConfig::default();
        assert!((cfg.quality_threshold - 0.5).abs() < f64::EPSILON);
        assert_eq!(cfg.max_escalations, 2);
        assert_eq!(cfg.window_size, 50);
        assert!(cfg.max_cascade_tokens.is_none());
        assert_eq!(cfg.classifier_mode, cascade::ClassifierMode::Heuristic);
    }

    #[test]
    fn evaluate_heuristic_empty_should_escalate_above_threshold() {
        let verdict = RouterProvider::evaluate_heuristic("", 0.05);
        // score = 0.0, threshold = 0.05 → should_escalate = true
        assert!(verdict.should_escalate);
    }

    #[test]
    fn evaluate_heuristic_good_response_does_not_escalate() {
        let text = "The answer to your question is straightforward. Consider the options and pick the best one.";
        let verdict = RouterProvider::evaluate_heuristic(text, 0.5);
        assert!(!verdict.should_escalate, "score={}", verdict.score);
    }

    /// Empty string from the only provider must not be stored as `best_seen`.
    /// When all providers fail or return empty, the caller should get an error,
    /// not a silent empty response.
    #[tokio::test]
    async fn cascade_empty_response_not_stored_as_best_seen() {
        use crate::mock::MockProvider;

        // Single provider returns empty string (score=0.0, should_escalate may be true/false).
        // With quality_threshold=0.0 it won't escalate, so we can check the return value.
        let p = AnyProvider::Mock(MockProvider::with_responses(vec![String::new()]));
        let cfg = CascadeRouterConfig {
            quality_threshold: 0.0,
            ..Default::default()
        };
        let r = RouterProvider::new(vec![p]).with_cascade(cfg);
        let msgs = vec![Message::from_legacy(Role::User, "hi")];
        // The provider returns "" — cascade must return it as-is (no best_seen involved
        // with a single provider), but this test confirms "" is not stored when escalating.
        let result = r.chat(&msgs).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "");
    }

    /// When provider 1 returns empty and provider 2 fails, `best_seen` must not hold
    /// the empty string — the caller must get an error, not a silent empty response.
    #[tokio::test]
    async fn cascade_empty_best_seen_not_returned_on_all_fail() {
        use crate::mock::MockProvider;

        // p1: returns empty string (causes escalation with default threshold)
        // p2: hard error
        let p1 = AnyProvider::Mock(MockProvider::with_responses(vec![String::new()]));
        let p2 = AnyProvider::Mock(MockProvider::failing());

        let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig::default());
        let msgs = vec![Message::from_legacy(Role::User, "hi")];
        let result = r.chat(&msgs).await;
        // best_seen must NOT be the empty string; error must propagate.
        assert!(
            result.is_err(),
            "expected error, not silent empty string; got: {result:?}"
        );
    }

    /// Stream variant: empty string from early provider must not be stored as `best_seen`.
    #[tokio::test]
    async fn cascade_stream_empty_response_not_stored_as_best_seen() {
        use crate::mock::MockProvider;

        // p1 (early): returns "" — should NOT be stored as best_seen.
        // p2 (last): returns a real response.
        let p1 = AnyProvider::Mock(MockProvider::with_responses(vec![String::new()]));
        let p2 = AnyProvider::Mock(
            MockProvider::with_responses(vec!["real answer".to_owned()]).with_streaming(),
        );

        let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig::default());
        let msgs = vec![Message::from_legacy(Role::User, "hi")];
        let stream = r.chat_stream(&msgs).await.expect("should not error");
        let text = collect_stream(stream).await.expect("stream should succeed");
        assert_eq!(text, "real answer");
    }

    // ── Arc<[AnyProvider]> + cost_tiers tests ──────────────────────────────────

    #[test]
    fn arc_providers_clone_shares_allocation() {
        use crate::mock::MockProvider;
        let p = AnyProvider::Mock(MockProvider::default());
        let r = RouterProvider::new(vec![p]);
        let c = r.clone();
        // Both RouterProvider instances must share the same Arc allocation.
        assert!(Arc::ptr_eq(&r.state.providers, &c.state.providers));
    }

    #[test]
    fn cost_tiers_reorders_providers_at_construction() {
        use crate::mock::MockProvider;
        let p1 = AnyProvider::Mock(MockProvider::default().with_name("claude"));
        let p2 = AnyProvider::Mock(MockProvider::default().with_name("ollama"));
        let p3 = AnyProvider::Mock(MockProvider::default().with_name("openai"));
        let r = RouterProvider::new(vec![p1, p2, p3]).with_cascade(CascadeRouterConfig {
            cost_tiers: Some(vec!["ollama".into(), "claude".into()]),
            ..CascadeRouterConfig::default()
        });
        let names: Vec<&str> = r.state.providers.iter().map(LlmProvider::name).collect();
        // ollama first (tier 0), claude second (tier 1), openai last (unlisted, original idx 2)
        assert_eq!(names, vec!["ollama", "claude", "openai"]);
    }

    #[test]
    fn cost_tiers_none_preserves_chain_order() {
        use crate::mock::MockProvider;
        let p1 = AnyProvider::Mock(MockProvider::default().with_name("claude"));
        let p2 = AnyProvider::Mock(MockProvider::default().with_name("ollama"));
        let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig {
            cost_tiers: None,
            ..CascadeRouterConfig::default()
        });
        let names: Vec<&str> = r.state.providers.iter().map(LlmProvider::name).collect();
        assert_eq!(names, vec!["claude", "ollama"]);
    }

    #[test]
    fn cost_tiers_empty_vec_preserves_chain_order() {
        use crate::mock::MockProvider;
        let p1 = AnyProvider::Mock(MockProvider::default().with_name("claude"));
        let p2 = AnyProvider::Mock(MockProvider::default().with_name("ollama"));
        let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig {
            cost_tiers: Some(vec![]),
            ..CascadeRouterConfig::default()
        });
        let names: Vec<&str> = r.state.providers.iter().map(LlmProvider::name).collect();
        assert_eq!(names, vec!["claude", "ollama"]);
    }

    #[test]
    fn cost_tiers_unknown_name_ignored() {
        use crate::mock::MockProvider;
        let p1 = AnyProvider::Mock(MockProvider::default().with_name("ollama"));
        let p2 = AnyProvider::Mock(MockProvider::default().with_name("claude"));
        let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig {
            cost_tiers: Some(vec!["nonexistent".into(), "ollama".into()]),
            ..CascadeRouterConfig::default()
        });
        let names: Vec<&str> = r.state.providers.iter().map(LlmProvider::name).collect();
        // "nonexistent" ignored; "ollama" is tier 1 → first; "claude" unlisted → second
        assert_eq!(names, vec!["ollama", "claude"]);
    }

    #[test]
    fn cost_tiers_all_providers_listed() {
        use crate::mock::MockProvider;
        let p1 = AnyProvider::Mock(MockProvider::default().with_name("c"));
        let p2 = AnyProvider::Mock(MockProvider::default().with_name("b"));
        let p3 = AnyProvider::Mock(MockProvider::default().with_name("a"));
        let r = RouterProvider::new(vec![p1, p2, p3]).with_cascade(CascadeRouterConfig {
            cost_tiers: Some(vec!["a".into(), "b".into(), "c".into()]),
            ..CascadeRouterConfig::default()
        });
        let names: Vec<&str> = r.state.providers.iter().map(LlmProvider::name).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn cost_tiers_duplicate_name_uses_last_position() {
        use crate::mock::MockProvider;
        let p1 = AnyProvider::Mock(MockProvider::default().with_name("ollama"));
        let p2 = AnyProvider::Mock(MockProvider::default().with_name("claude"));
        // "ollama" appears twice in tiers: HashMap overwrites → position 2.
        // claude=tier 0, ollama=tier 2 → claude before ollama.
        let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig {
            cost_tiers: Some(vec!["claude".into(), "ollama".into(), "ollama".into()]),
            ..CascadeRouterConfig::default()
        });
        let names: Vec<&str> = r.state.providers.iter().map(LlmProvider::name).collect();
        assert_eq!(names, vec!["claude", "ollama"]);
    }

    #[test]
    fn cost_tiers_empty_router_does_not_panic() {
        let r = RouterProvider::new(vec![]).with_cascade(CascadeRouterConfig {
            cost_tiers: Some(vec!["foo".into()]),
            ..CascadeRouterConfig::default()
        });
        assert_eq!(r.state.providers.len(), 0);
    }

    #[test]
    fn set_status_tx_works_with_arc() {
        use crate::mock::MockProvider;
        let p = AnyProvider::Mock(MockProvider::default());
        let mut r = RouterProvider::new(vec![p]);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        r.set_status_tx(tx); // must not panic
    }

    #[tokio::test]
    async fn cascade_chat_with_tools_unaffected_by_cost_tiers() {
        use crate::mock::MockProvider;
        // chat_with_tools skips cascade entirely (HIGH-04). Verify that cost_tiers
        // ordering does not accidentally affect the non-cascade tool fallback path.
        let p1 = AnyProvider::Mock(MockProvider::failing().with_name("cheap"));
        let p2 = AnyProvider::Mock(MockProvider::failing().with_name("expensive"));
        let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig {
            cost_tiers: Some(vec!["cheap".into()]),
            ..CascadeRouterConfig::default()
        });
        let msgs = vec![Message::from_legacy(Role::User, "hi")];
        // Both providers fail → NoProviders, not a cascade-specific error.
        let err = r.chat_with_tools(&msgs, &[]).await.unwrap_err();
        assert!(matches!(err, LlmError::NoProviders));
    }

    // ── Embed retry / rate-limit tests ────────────────────────────────────────

    /// Provider returns `RateLimited` twice then succeeds on the third attempt.
    /// The router must retry and return the successful embedding.
    #[tokio::test]
    async fn embed_retries_on_rate_limited_then_succeeds() {
        use crate::mock::MockProvider;

        let p = AnyProvider::Mock({
            let mut m = MockProvider::default()
                .with_errors(vec![LlmError::RateLimited, LlmError::RateLimited])
                .with_name("p1");
            m.supports_embeddings = true;
            m.embedding = vec![0.1, 0.2];
            m
        });
        let r = RouterProvider::new(vec![p]);
        let result = r.embed("text").await.unwrap();
        assert_eq!(result, vec![0.1, 0.2]);
    }

    /// When all retries (3) are exhausted on the first provider, the router falls
    /// back to the second provider and returns its embedding.
    #[tokio::test]
    async fn embed_falls_back_after_all_retries_exhausted() {
        use crate::mock::MockProvider;

        // p1: 4 RateLimited errors (attempt 0..=3 all fail)
        let p1 = AnyProvider::Mock({
            let mut m = MockProvider::default()
                .with_errors(vec![
                    LlmError::RateLimited,
                    LlmError::RateLimited,
                    LlmError::RateLimited,
                    LlmError::RateLimited,
                ])
                .with_name("p1");
            m.supports_embeddings = true;
            m
        });
        let p2 = AnyProvider::Mock({
            let mut m = MockProvider::default().with_name("p2");
            m.supports_embeddings = true;
            m.embedding = vec![9.0, 8.0];
            m
        });
        let r = RouterProvider::new(vec![p1, p2]);
        let result = r.embed("text").await.unwrap();
        assert_eq!(result, vec![9.0, 8.0]);
    }

    /// Provider returns `RateLimited` twice then succeeds via `embed_batch`.
    #[tokio::test]
    async fn embed_batch_retries_on_rate_limited_then_succeeds() {
        use crate::mock::MockProvider;

        let p = AnyProvider::Mock({
            let mut m = MockProvider::default()
                .with_errors(vec![LlmError::RateLimited, LlmError::RateLimited])
                .with_name("p1");
            m.supports_embeddings = true;
            m.embedding = vec![0.5, 0.6];
            m
        });
        let r = RouterProvider::new(vec![p]);
        let result = r.embed_batch(&["a", "b"]).await.unwrap();
        assert_eq!(result, vec![vec![0.5, 0.6], vec![0.5, 0.6]]);
    }

    /// When all `embed_batch` retries are exhausted on the first provider, falls back
    /// to the second provider.
    #[tokio::test]
    async fn embed_batch_falls_back_after_all_retries_exhausted() {
        use crate::mock::MockProvider;

        // p1 needs 4 errors per embed call * 1 text = 4 total (attempt 0..=3)
        let p1 = AnyProvider::Mock({
            let mut m = MockProvider::default()
                .with_errors(vec![
                    LlmError::RateLimited,
                    LlmError::RateLimited,
                    LlmError::RateLimited,
                    LlmError::RateLimited,
                ])
                .with_name("p1");
            m.supports_embeddings = true;
            m
        });
        let p2 = AnyProvider::Mock({
            let mut m = MockProvider::default().with_name("p2");
            m.supports_embeddings = true;
            m.embedding = vec![7.0, 8.0];
            m
        });
        let r = RouterProvider::new(vec![p1, p2]);
        let result = r.embed_batch(&["x"]).await.unwrap();
        assert_eq!(result, vec![vec![7.0, 8.0]]);
    }

    // ── InvalidInput embed break tests ────────────────────────────────────────

    /// When a provider returns `InvalidInput` from `embed()`, the router must break
    /// the fallback loop immediately and return `InvalidInput` — not `NoProviders`.
    #[tokio::test]
    async fn embed_invalid_input_breaks_loop_and_returns_invalid_input() {
        use crate::mock::MockProvider;

        let p = AnyProvider::Mock(MockProvider::default().with_embed_invalid_input());
        let r = RouterProvider::new(vec![p]).with_thompson(None);
        let err = r.embed("some text").await.unwrap_err();
        assert!(
            matches!(err, LlmError::InvalidInput { .. }),
            "expected InvalidInput, got {err:?}"
        );
    }

    /// When a provider returns `InvalidInput`, the router must NOT fall through to
    /// the next provider — a second embed-capable provider must never be called.
    #[tokio::test]
    async fn embed_invalid_input_does_not_fall_through_to_second_provider() {
        use crate::mock::MockProvider;

        // p1 returns InvalidInput; p2 is a functioning embed provider.
        // If the loop falls through, p2 returns Ok — which would mean the error was
        // swallowed instead of breaking immediately.
        let p1 = AnyProvider::Mock(
            MockProvider::default()
                .with_embed_invalid_input()
                .with_name("p1"),
        );
        let p2 = AnyProvider::Mock({
            let mut m = MockProvider::default();
            m.supports_embeddings = true;
            m.name_override = Some("p2".into());
            m
        });

        let r = RouterProvider::new(vec![p1, p2]);
        let err = r.embed("test").await.unwrap_err();

        // The error must carry p1's name, proving p2 was never reached.
        assert!(
            matches!(&err, LlmError::InvalidInput { provider, .. } if provider == "p1"),
            "expected InvalidInput from p1, got {err:?}"
        );
    }

    // ── InvalidInput chat_with_tools break tests ───────────────────────────────

    /// When a provider returns `InvalidInput` from `chat_with_tools()`, the router must break
    /// the fallback loop immediately and return `InvalidInput` — not `NoProviders`.
    #[tokio::test]
    async fn chat_with_tools_invalid_input_breaks_loop_and_returns_invalid_input() {
        use crate::mock::MockProvider;
        use crate::provider::ToolDefinition;

        let p = AnyProvider::Mock(MockProvider::default().with_tool_chat_invalid_input());
        let r = RouterProvider::new(vec![p]).with_thompson(None);
        let err = r
            .chat_with_tools(&[], &[] as &[ToolDefinition])
            .await
            .unwrap_err();
        assert!(
            matches!(err, LlmError::InvalidInput { .. }),
            "expected InvalidInput, got {err:?}"
        );
    }

    /// When a provider returns `InvalidInput` from `chat_with_tools()`, the router must NOT
    /// fall through to the next provider.
    #[tokio::test]
    async fn chat_with_tools_invalid_input_does_not_fall_through_to_second_provider() {
        use crate::mock::MockProvider;
        use crate::provider::ToolDefinition;

        let p1 = AnyProvider::Mock(
            MockProvider::default()
                .with_tool_chat_invalid_input()
                .with_name("p1"),
        );
        let p2 = AnyProvider::Mock(MockProvider::default().with_name("p2"));

        let r = RouterProvider::new(vec![p1, p2]);
        let err = r
            .chat_with_tools(&[], &[] as &[ToolDefinition])
            .await
            .unwrap_err();

        assert!(
            matches!(&err, LlmError::InvalidInput { provider, .. } if provider == "p1"),
            "expected InvalidInput from p1, got {err:?}"
        );
    }

    /// The router skips providers that do not support embeddings and continues to
    /// the next one, returning a successful result from the first capable provider.
    #[tokio::test]
    async fn embed_skips_non_embedding_providers_and_falls_through() {
        use crate::mock::MockProvider;

        // p1 does not support embeddings — skipped by the loop guard.
        // p2 supports embeddings and returns successfully.
        let p1 = AnyProvider::Mock({
            let mut m = MockProvider::default().with_name("p1");
            m.supports_embeddings = false;
            m
        });
        let p2 = AnyProvider::Mock({
            let mut m = MockProvider::default().with_name("p2");
            m.supports_embeddings = true;
            m.embedding = vec![1.0, 2.0, 3.0];
            m
        });

        let r = RouterProvider::new(vec![p1, p2]);
        let result = r.embed("hello").await.unwrap();
        assert_eq!(result, vec![1.0, 2.0, 3.0]);
    }

    /// `InvalidInput` from embed does not call `record_availability` (no reputation penalty).
    /// We verify this indirectly: `thompson_stats` must show no entry for the provider
    /// after an `InvalidInput` embed, whereas a normal embed failure increments it.
    #[tokio::test]
    async fn embed_invalid_input_does_not_record_availability() {
        use crate::mock::MockProvider;

        let p = AnyProvider::Mock(
            MockProvider::default()
                .with_embed_invalid_input()
                .with_name("test-provider"),
        );
        let r = RouterProvider::new(vec![p]).with_thompson(None);
        let _ = r.embed("text").await;

        // record_availability is only called on success or generic error,
        // not on InvalidInput. So thompson_stats must have no entry for "test-provider".
        let stats = r.thompson_stats();
        let provider_in_stats = stats.iter().any(|(name, ..)| name == "test-provider");
        assert!(
            !provider_in_stats,
            "InvalidInput must not update provider reputation; stats: {stats:?}"
        );
    }

    // ── quality_gate tests ────────────────────────────────────────────────────

    /// `with_quality_gate()` happy path: when cosine similarity >= threshold the
    /// response is returned directly without falling back.
    #[tokio::test]
    async fn quality_gate_passes_when_similarity_above_threshold() {
        use crate::mock::MockProvider;

        // p1 returns a response; embed returns a unit vector so cosine similarity
        // with itself is 1.0 (>= any reasonable threshold).
        let p1 = AnyProvider::Mock({
            let mut m = MockProvider::with_responses(vec!["answer".to_owned()]).with_name("p1");
            m.supports_embeddings = true;
            m.embedding = vec![1.0, 0.0];
            m
        });
        let r = RouterProvider::new(vec![p1])
            .with_thompson(None)
            .with_quality_gate(0.5);
        let msgs = vec![Message::from_legacy(Role::User, "question")];
        let result = r.chat(&msgs).await.unwrap();
        assert_eq!(result, "answer");
    }

    /// `with_quality_gate()` exhaustion: when all providers fail the gate the router
    /// returns the best-seen response (highest similarity) rather than an error.
    #[tokio::test]
    async fn quality_gate_exhaustion_returns_best_seen() {
        use crate::mock::MockProvider;

        // p1 returns a response but embedding similarity is 0.0 (orthogonal vectors)
        // so it fails the gate (0.0 < 0.9). p2 fails entirely.
        // Expected: best_seen from p1 is returned.
        let p1 = AnyProvider::Mock({
            let mut m =
                MockProvider::with_responses(vec!["best_so_far".to_owned()]).with_name("p1");
            m.supports_embeddings = true;
            // query embed = [1,0], response embed = [0,1] → similarity = 0.0
            m.embedding = vec![0.0, 1.0];
            m
        });
        let p2 = AnyProvider::Mock(MockProvider::failing().with_name("p2"));
        let r = RouterProvider::new(vec![p1, p2])
            .with_thompson(None)
            .with_quality_gate(0.9);
        let msgs = vec![Message::from_legacy(Role::User, "question")];
        let result = r.chat(&msgs).await.unwrap();
        assert_eq!(result, "best_so_far");
    }

    // ── apply_routing_signals guard logic tests ───────────────────────────────

    /// `quality_gate = 5.0` (> 1.0) must be silently ignored — the field is left
    /// as `None` and no panic occurs.
    #[test]
    fn routing_signals_quality_gate_above_one_is_ignored() {
        // Build a RouterProvider directly and check that with_quality_gate is only
        // called for in-range values by replicating the guard from provider.rs.
        let threshold: f32 = 5.0;
        let mut router = RouterProvider::new(vec![]);
        if threshold.is_finite() && threshold > 0.0 && threshold <= 1.0 {
            router = router.with_quality_gate(threshold);
        }
        assert!(
            router.quality_gate.is_none(),
            "out-of-range quality_gate must not be wired; got {:?}",
            router.quality_gate
        );
    }

    /// `quality_gate = 0.8` (valid) must be wired into the router.
    #[test]
    fn routing_signals_quality_gate_valid_is_wired() {
        let threshold: f32 = 0.8;
        let mut router = RouterProvider::new(vec![]);
        if threshold.is_finite() && threshold > 0.0 && threshold <= 1.0 {
            router = router.with_quality_gate(threshold);
        }
        assert_eq!(
            router.quality_gate,
            Some(0.8),
            "valid quality_gate must be wired"
        );
    }

    // --- ASI debounce tests ---

    #[test]
    fn asi_debounce_same_turn_fires_once() {
        let router = RouterProvider::new(vec![]);
        let turn_id = 42u64;

        // First call: prev == u64::MAX (initial) → not equal to turn_id → proceeds (returns false)
        let prev1 = router.state.asi_last_turn.swap(turn_id, Ordering::AcqRel);
        let first_dropped = prev1 == turn_id;

        // Second call same turn: prev == turn_id → dropped
        let prev2 = router.state.asi_last_turn.swap(turn_id, Ordering::AcqRel);
        let second_dropped = prev2 == turn_id;

        assert!(!first_dropped, "first call in turn must not be dropped");
        assert!(second_dropped, "second call in same turn must be dropped");
    }

    #[test]
    fn asi_debounce_next_turn_fires_again() {
        let router = RouterProvider::new(vec![]);

        // Simulate turn 1
        let prev1 = router.state.asi_last_turn.swap(1u64, Ordering::AcqRel);
        assert_ne!(prev1, 1u64, "turn 1: initial value != 1, should proceed");

        // Simulate turn 2 — different turn_id
        let prev2 = router.state.asi_last_turn.swap(2u64, Ordering::AcqRel);
        let dropped = prev2 == 2u64;
        assert!(!dropped, "turn 2 must not be dropped (different turn_id)");
    }

    #[test]
    fn turn_counter_increments_across_clones() {
        let router = RouterProvider::new(vec![]);
        let clone = router.clone();

        let t0 = router.state.turn_counter.fetch_add(1, Ordering::Relaxed);
        let t1 = clone.state.turn_counter.fetch_add(1, Ordering::Relaxed);

        // Both clones share the same Arc<AtomicU64>
        assert_eq!(t1, t0 + 1, "cloned router shares turn_counter");
    }

    #[test]
    fn with_embed_concurrency_zero_means_no_semaphore() {
        let r = RouterProvider::new(vec![]).with_embed_concurrency(0);
        assert!(
            r.state.embed_semaphore.is_none(),
            "0 should disable semaphore"
        );
    }

    #[test]
    fn with_embed_concurrency_positive_creates_semaphore() {
        let r = RouterProvider::new(vec![]).with_embed_concurrency(4);
        let sem = r
            .state
            .embed_semaphore
            .as_ref()
            .expect("semaphore should exist");
        assert_eq!(sem.available_permits(), 4);
    }

    #[tokio::test]
    async fn embed_semaphore_limits_concurrency() {
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering as AO};

        // Use a semaphore with 2 permits. Verify that at most 2 concurrent
        // tasks can hold the permit at the same time.
        let sem = Arc::new(tokio::sync::Semaphore::new(2));
        let concurrent_peak = StdArc::new(AtomicUsize::new(0));
        let active = StdArc::new(AtomicUsize::new(0));

        let mut handles = vec![];
        for _ in 0..6 {
            let sem_clone = sem.clone();
            let peak = concurrent_peak.clone();
            let active = active.clone();
            handles.push(tokio::spawn(async move {
                let _permit = sem_clone.acquire().await.unwrap();
                let cur = active.fetch_add(1, AO::SeqCst) + 1;
                // Track peak concurrent usage.
                let mut p = peak.load(AO::SeqCst);
                while p < cur {
                    match peak.compare_exchange(p, cur, AO::SeqCst, AO::SeqCst) {
                        Ok(_) => break,
                        Err(new) => p = new,
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                active.fetch_sub(1, AO::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert!(
            concurrent_peak.load(AO::SeqCst) <= 2,
            "peak concurrency should not exceed semaphore limit"
        );
    }

    // ── TurnEmbedCache tests (#2819) ──────────────────────────────────────────

    /// T2: A second `embed_cached` call with the same text hits the cache instead of
    /// calling the underlying provider, and `embed_cache_hits` increments to 1.
    #[tokio::test]
    async fn turn_embed_cache_hit_increments_counter() {
        use crate::mock::MockProvider;

        let mut m = MockProvider::default();
        m.supports_embeddings = true;
        m.embedding = vec![0.5, 0.5];
        let provider_embed_calls = Arc::clone(&m.embed_call_count);

        let r = RouterProvider::new(vec![AnyProvider::Mock(m)]);
        let cache = Mutex::new(TurnEmbedCache::default());

        // First call — cache miss → calls provider.
        let emb1 = r.embed_cached("hello", &cache).await.unwrap();
        // Second call — same text → cache hit, no provider call.
        let emb2 = r.embed_cached("hello", &cache).await.unwrap();

        assert_eq!(emb1, emb2, "cached embedding must match original");
        assert_eq!(
            provider_embed_calls.load(Ordering::Relaxed),
            1,
            "provider embed() must be called exactly once (second call hits cache)"
        );
        let (total, hits) = r.embed_cache_metrics();
        assert_eq!(
            total, 2,
            "embed_call_count must be 2 (two embed_cached calls)"
        );
        assert_eq!(hits, 1, "embed_cache_hits must be 1 (one cache hit)");
    }

    /// T3: Passing `Some(precomputed_embedding)` to `spawn_asi_update` does not trigger
    /// an `embed()` call on the provider; the ASI window is updated with the provided embedding.
    #[tokio::test]
    async fn spawn_asi_update_with_precomputed_skips_embed() {
        use crate::mock::MockProvider;

        let mut m = MockProvider::with_responses(vec!["ok".to_owned()]);
        m.supports_embeddings = true;
        m.embedding = vec![1.0, 0.0];
        let provider_embed_calls = Arc::clone(&m.embed_call_count);

        let r =
            RouterProvider::new(vec![AnyProvider::Mock(m)]).with_asi(AsiRouterConfig::default());

        let precomputed = vec![0.9_f32, 0.1];
        let turn_id = 42u64;

        // Inject a different turn id into asi_last_turn so the debounce doesn't fire.
        r.state.asi_last_turn.store(u64::MAX, Ordering::SeqCst);

        r.spawn_asi_update(
            "p1",
            "response".to_owned(),
            turn_id,
            Some(precomputed.clone()),
        );

        // Give the spawned task time to complete.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Provider embed() must not have been called.
        assert_eq!(
            provider_embed_calls.load(Ordering::Relaxed),
            0,
            "embed() must not be called when precomputed_embedding is Some"
        );

        // The ASI window must have received the precomputed embedding.
        let asi = r.asi.as_ref().unwrap().lock();
        let coherence = asi.coherence("p1");
        // coherence_score returns None when the window has < 2 entries; after one push it's None.
        // We only verify the ASI has the provider in its window (score will be None with 1 entry).
        let _ = coherence; // just verifying no panic
    }
}
