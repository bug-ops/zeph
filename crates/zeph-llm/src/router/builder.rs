// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Construction and configuration of [`RouterProvider`].
//!
//! Holds the constructor, the strategy builder methods (`with_*`), state-persistence
//! helpers (`save_*`), and the diagnostic accessors (`*_stats`, `set_status_tx`,
//! `list_models_remote`).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use parking_lot::Mutex;

use super::asi::AsiState;
use super::bandit::BanditState;
use super::cascade::CascadeState;
use super::coe::{self, CoeRouter};
use super::config::{AsiRouterConfig, BanditRouterConfig, CascadeRouterConfig};
use super::embed_cache::BanditEmbedCache;
use super::reputation::{self, ReputationTracker};
use super::state::RouterState;
use super::thompson::{self, ThompsonState};
use super::{RouterProvider, RouterStrategy, blocking_load};
use crate::any::AnyProvider;
use crate::ema::EmaTracker;
use crate::error::LlmError;
use crate::provider::{LlmProvider, StatusTx};

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
            embed_timeout_ms: 5000,
            asi_tasks: Arc::new(Mutex::new(tokio::task::JoinSet::new())),
        }
    }

    /// Set the per-call timeout for [`embed`][Self::embed] across all non-bandit providers.
    ///
    /// A stalled provider is skipped and the next candidate is tried. Default is `5000` ms.
    /// Pass `0` to disable the timeout (not recommended for production).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zeph_llm::router::RouterProvider;
    /// let router = RouterProvider::new(vec![]).with_embed_timeout(3000);
    /// ```
    #[must_use]
    pub fn with_embed_timeout(mut self, timeout_ms: u64) -> Self {
        self.embed_timeout_ms = timeout_ms;
        self
    }

    /// Register the provider explicitly flagged `embed = true` in `[[llm.providers]]`.
    ///
    /// `embed()`/`embed_batch()` try this provider first, ahead of the generic scan over
    /// `providers` for any backend that merely reports `supports_embeddings() == true`.
    /// Without this, a chat-only provider sharing a backend type with the dedicated
    /// embedding provider (e.g. two `OllamaProvider` instances) can shadow it and silently
    /// fall back to an unconfigured default embedding model (#5859).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zeph_llm::router::RouterProvider;
    /// # use zeph_llm::any::AnyProvider;
    /// # use zeph_llm::ollama::OllamaProvider;
    /// let embedder = AnyProvider::Ollama(
    ///     OllamaProvider::new("http://localhost:11434", "chat-model".into(), "nomic-embed-text".into())
    ///         .with_provider_name("embedder"),
    /// );
    /// let router = RouterProvider::new(vec![]).with_embed_provider(embedder);
    /// ```
    #[must_use]
    pub fn with_embed_provider(mut self, provider: AnyProvider) -> Self {
        self.state.dedicated_embed_provider = Some(Arc::new(provider));
        self
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
        let raw = confidence.map_or(u32::MAX, f32::to_bits);
        self.state
            .last_memory_confidence
            .store(raw, std::sync::atomic::Ordering::Relaxed);
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
            secondary: Arc::new(secondary) as Arc<dyn crate::provider_dyn::LlmProviderDyn>,
            embed: Arc::new(embed) as Arc<dyn crate::provider_dyn::LlmProviderDyn>,
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
        let mut state = blocking_load(|| ThompsonState::load(&path));
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
    /// Loads existing state from `state_path` (or the default path) using
    /// [`tokio::task::block_in_place`] to avoid blocking the async executor.
    /// Applies session-level decay if `config.decay_factor < 1.0`, and prunes arms for
    /// removed providers.
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
        let mut state = blocking_load(|| BanditState::load(&path));
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
        self.bandit_embedding_provider =
            embedding_provider.map(|p| Arc::new(p) as Arc<dyn crate::provider_dyn::LlmProviderDyn>);
        // Initialize Thompson state for cold-start fallback (total_updates < warmup_queries).
        // Uses default uniform priors; no persistence path needed since it's a fallback only.
        self.thompson = Some(Arc::new(Mutex::new(ThompsonState::default())));
        self.bandit_config = Some(config);
        self
    }

    /// Persist current bandit state to disk. No-op if bandit strategy is not active.
    ///
    /// Uses [`tokio::task::spawn_blocking`] so it is safe to call from any async context.
    #[tracing::instrument(name = "llm.router.builder.save_bandit_state", skip_all)]
    pub async fn save_bandit_state(&self) {
        let (Some(bandit), Some(path)) = (&self.bandit, &self.bandit_state_path) else {
            return;
        };
        let bandit = Arc::clone(bandit);
        let path = path.clone();
        tokio::task::spawn_blocking(move || {
            let state = bandit.lock();
            if let Err(e) = state.save(&path) {
                tracing::warn!(error = %e, "failed to save bandit state");
            }
        })
        .await
        .unwrap_or_else(|e| tracing::warn!(error = %e, "bandit state save task panicked"));
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
    /// Loads existing state from `state_path` (or the default path) using
    /// [`tokio::task::block_in_place`] to avoid blocking the async executor.
    /// Applies session-level decay and prunes stale provider entries.
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
        let mut tracker = blocking_load(|| ReputationTracker::load(&path));
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

    /// Returns the `provider_kind_str` of the last provider selected by the router.
    ///
    /// Used by [`crate::any::AnyProvider::provider_kind_str`] to attribute cost to the
    /// actual child provider rather than returning the generic `"local"` sentinel for all
    /// router-dispatched calls. Falls back to `"local"` when no call has been made yet.
    #[must_use]
    pub fn last_selected_provider_kind(&self) -> &'static str {
        let name = self.state.last_active_provider.lock().clone();
        let Some(name) = name else {
            return "local";
        };
        self.state
            .providers
            .iter()
            .find(|p| p.name() == name)
            .map_or("local", |p| p.provider_kind_str())
    }

    /// Persist current reputation state to disk. No-op if reputation is disabled.
    /// Uses [`tokio::task::spawn_blocking`] so it is safe to call from any async context.
    #[tracing::instrument(name = "llm.router.builder.save_reputation_state", skip_all)]
    pub async fn save_reputation_state(&self) {
        let (Some(reputation), Some(path)) = (&self.reputation, &self.reputation_state_path) else {
            return;
        };
        let reputation = Arc::clone(reputation);
        let path = path.clone();
        tokio::task::spawn_blocking(move || {
            let state = reputation.lock();
            if let Err(e) = state.save(&path) {
                tracing::warn!(error = %e, "failed to save reputation state");
            }
        })
        .await
        .unwrap_or_else(|e| tracing::warn!(error = %e, "reputation state save task panicked"));
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
    /// after listed ones in original chain order. Unknown names in `cost_tiers` emit
    /// a warning and are otherwise ignored.
    #[must_use]
    pub fn with_cascade(mut self, config: CascadeRouterConfig) -> Self {
        self.strategy = RouterStrategy::Cascade;

        if let Some(ref tiers) = config.cost_tiers
            && !tiers.is_empty()
        {
            let provider_names: std::collections::HashSet<&str> =
                self.state.providers.iter().map(AnyProvider::name).collect();
            for name in tiers {
                if !provider_names.contains(name.as_str()) {
                    tracing::warn!(
                        name = %name,
                        "cascade: cost_tiers entry does not match any provider name"
                    );
                }
            }

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
    /// Uses [`tokio::task::spawn_blocking`] so it is safe to call from any async context,
    /// including mid-request paths.
    #[tracing::instrument(name = "llm.router.builder.save_thompson_state", skip_all)]
    pub async fn save_thompson_state(&self) {
        let (Some(thompson), Some(path)) = (&self.thompson, &self.thompson_state_path) else {
            return;
        };
        let thompson = Arc::clone(thompson);
        let path = path.clone();
        tokio::task::spawn_blocking(move || {
            let state = thompson.lock();
            if let Err(e) = state.save(&path) {
                tracing::warn!(error = %e, "failed to save Thompson router state");
            }
        })
        .await
        .unwrap_or_else(|e| tracing::warn!(error = %e, "Thompson state save task panicked"));
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

    /// Resolve the pool index that runtime capability commands (`/think-tokens`,
    /// `/reasoning-effort`) target: the inner provider that served the most recent call, or
    /// the first configured provider as a deterministic fallback (FR-006) when no dispatch has
    /// happened yet this session, or when `last_active_provider` names a provider no longer in
    /// the pool (config drift). Returns `None` only for an empty pool.
    pub(crate) fn capability_target_index(&self) -> Option<usize> {
        if self.state.providers.is_empty() {
            return None;
        }
        let name = self.state.last_active_provider.lock().clone();
        match name {
            Some(name) => Some(
                self.state
                    .providers
                    .iter()
                    .position(|p| p.name() == name)
                    .unwrap_or(0),
            ),
            None => Some(0),
        }
    }

    /// Mutate the pooled provider at `idx` in place.
    ///
    /// `RouterState::providers` is `Arc<[AnyProvider]>`; `Arc::get_mut` succeeds at refcount 1
    /// (the common case for a slash command holding `&mut` to the sole owned
    /// `AnyProvider::Router`). The rebuild branch below is **not** a defensive fallback: any
    /// in-flight `spawn_asi_update` background task clones `RouterProvider` (sharing this same
    /// `providers` Arc) for up to `embed_timeout_ms` (default 5000ms) after every turn, so a
    /// `/think-tokens`/`/reasoning-effort` issued shortly after a turn routinely takes this
    /// path. The stale clone keeps its old (transient) slice; the authoritative router's *next*
    /// dispatch reads the freshly rebuilt Arc, so the mutation still persists (FR-007).
    ///
    /// Correctness invariant: this mutates the authoritative `RouterProvider` instance. Dispatch
    /// (`chat`/`chat_with_tools`) reads `self.state` on the same authoritative instance the
    /// agent holds, so a setter call landing before the next dispatch is guaranteed to be
    /// observed by it. A future refactor that caches a pre-cloned dispatch copy ahead of time
    /// would silently break this.
    fn with_target_provider_mut<T>(
        &mut self,
        idx: usize,
        f: impl FnOnce(&mut AnyProvider) -> T,
    ) -> T {
        if let Some(providers) = Arc::get_mut(&mut self.state.providers) {
            f(&mut providers[idx])
        } else {
            let mut v: Vec<_> = self.state.providers.iter().cloned().collect();
            let out = f(&mut v[idx]);
            self.state.providers = Arc::from(v);
            out
        }
    }

    /// Delegated implementation of [`crate::any::AnyProvider::set_thinking_budget`] for
    /// `Self::Router`. See [`Self::capability_target_index`] for target resolution.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::ModelCapabilityMismatch`] naming `"router"` when the pool is empty,
    /// or the real inner provider's own error when it does not support a thinking-token budget.
    pub(crate) fn set_thinking_budget_delegated(
        &mut self,
        budget: Option<u32>,
    ) -> Result<(), LlmError> {
        let idx =
            self.capability_target_index()
                .ok_or_else(|| LlmError::ModelCapabilityMismatch {
                    provider: "router".to_owned(),
                    message: "router has no configured providers".into(),
                })?;
        tracing::debug!(
            target_idx = idx,
            strategy = ?self.strategy,
            "router: delegating set_thinking_budget to applicable inner provider"
        );
        self.with_target_provider_mut(idx, |p| p.set_thinking_budget(budget))
    }

    /// Delegated implementation of [`crate::any::AnyProvider::apply_reasoning_effort`] for
    /// `Self::Router`. See [`Self::capability_target_index`] for target resolution.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::ModelCapabilityMismatch`] naming `"router"` when the pool is empty,
    /// or the real inner provider's own error when it does not support a reasoning-effort level.
    pub(crate) fn apply_reasoning_effort_delegated(
        &mut self,
        effort: crate::any::ReasoningEffort,
    ) -> Result<(), LlmError> {
        let idx =
            self.capability_target_index()
                .ok_or_else(|| LlmError::ModelCapabilityMismatch {
                    provider: "router".to_owned(),
                    message: "router has no configured providers".into(),
                })?;
        tracing::debug!(
            target_idx = idx,
            strategy = ?self.strategy,
            "router: delegating apply_reasoning_effort to applicable inner provider"
        );
        self.with_target_provider_mut(idx, |p| p.apply_reasoning_effort(effort))
    }

    /// Delegated implementation of [`crate::any::AnyProvider::current_thinking_budget`] for
    /// `Self::Router`.
    #[must_use]
    pub(crate) fn current_thinking_budget_delegated(&self) -> Option<u32> {
        let idx = self.capability_target_index()?;
        self.state.providers[idx].current_thinking_budget()
    }

    /// Delegated implementation of [`crate::any::AnyProvider::current_reasoning_effort`] for
    /// `Self::Router`.
    #[must_use]
    pub(crate) fn current_reasoning_effort_delegated(&self) -> Option<String> {
        let idx = self.capability_target_index()?;
        self.state.providers[idx].current_reasoning_effort()
    }

    /// Delegated implementation of [`crate::any::AnyProvider::capability_delegation_advisory`]
    /// for `Self::Router`.
    ///
    /// Returns `None` when the pool has at most one provider, or when `self.strategy` is
    /// [`RouterStrategy::Cascade`] (deterministic cheapest-first — always reselects the same
    /// slot barring quality-driven escalation). For re-sampling strategies (`Ema`, `Thompson`,
    /// `Bandit`) over a multi-provider pool, the next dispatch may pick a different provider
    /// than the one just configured — see spec `071-router-thinking-budget-delegation` §5.
    #[must_use]
    pub(crate) fn capability_delegation_advisory(&self) -> Option<String> {
        if self.state.providers.len() <= 1 || self.strategy == RouterStrategy::Cascade {
            return None;
        }
        let idx = self.capability_target_index()?;
        let name = self.state.providers.get(idx)?.name();
        Some(format!(
            "applied to {name}; routing={:?} may select a different provider on the next turn",
            self.strategy
        ))
    }

    /// Aggregate model lists from all sub-providers, deduplicating by id.
    ///
    /// Individual sub-provider errors are logged as warnings and skipped.
    ///
    /// # Errors
    ///
    /// Always succeeds (errors per-provider are swallowed).
    #[tracing::instrument(name = "llm.router.builder.list_models_remote", skip_all)]
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
}
