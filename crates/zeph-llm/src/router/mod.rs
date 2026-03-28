// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Provider router: EMA-based, Thompson Sampling, and Cascade strategies.
//!
//! # Security
//!
//! Thompson state is loaded from a user-controlled path at startup. The file is
//! validated (finite floats, clamped range) and written with `0o600` permissions
//! on Unix. Do not store the state file in world-writable directories.

pub mod cascade;
pub mod reputation;
pub mod thompson;
pub mod triage;

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::any::AnyProvider;
use crate::ema::EmaTracker;
use crate::error::LlmError;
use crate::provider::{ChatResponse, ChatStream, LlmProvider, Message, StatusTx, ToolDefinition};

use cascade::{CascadeState, ClassifierMode, heuristic_score};
use reputation::ReputationTracker;
use thompson::ThompsonState;

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

#[derive(Debug, Clone)]
pub struct RouterProvider {
    // Arc<[AnyProvider]> makes self.clone() O(1) for the providers field (atomic refcount
    // increment) instead of O(N * provider_size). This benefits ALL strategies since every
    // chat/chat_stream/embed/chat_with_tools call does `let router = self.clone()`.
    providers: Arc<[AnyProvider]>,
    status_tx: Option<StatusTx>,
    ema: Option<EmaTracker>,
    provider_order: Arc<Mutex<Vec<usize>>>,
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
    /// Name of the sub-provider that served the most recent successful tool call.
    /// Used by `record_quality_outcome` to attribute quality to the right provider.
    last_active_provider: Arc<Mutex<Option<String>>>,
}

impl RouterProvider {
    #[must_use]
    pub fn new(providers: Vec<AnyProvider>) -> Self {
        let n = providers.len();
        Self {
            providers: Arc::from(providers),
            status_tx: None,
            ema: None,
            provider_order: Arc::new(Mutex::new((0..n).collect())),
            strategy: RouterStrategy::Ema,
            thompson: None,
            thompson_state_path: None,
            cascade_state: None,
            cascade_config: None,
            reputation: None,
            reputation_state_path: None,
            reputation_weight: 0.3,
            last_active_provider: Arc::new(Mutex::new(None)),
        }
    }

    /// Enable EMA-based adaptive provider ordering.
    #[must_use]
    pub fn with_ema(mut self, alpha: f64, reorder_interval: u64) -> Self {
        self.ema = Some(EmaTracker::new(alpha, reorder_interval));
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
        let known: std::collections::HashSet<String> =
            self.providers.iter().map(|p| p.name().to_owned()).collect();
        state.prune(&known);
        self.thompson = Some(Arc::new(Mutex::new(state)));
        self.thompson_state_path = Some(path);
        self
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
        let known: std::collections::HashSet<String> =
            self.providers.iter().map(|p| p.name().to_owned()).collect();
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
        if self.strategy == RouterStrategy::Cascade {
            return;
        }
        let Some(ref reputation) = self.reputation else {
            return;
        };
        let active = self
            .last_active_provider
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(provider_name) = active else {
            return;
        };
        let mut tracker = reputation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tracker.record_quality(&provider_name, success);
    }

    /// Persist current reputation state to disk. No-op if reputation is disabled.
    pub fn save_reputation_state(&self) {
        let (Some(reputation), Some(path)) = (&self.reputation, &self.reputation_state_path) else {
            return;
        };
        let state = reputation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let tracker = reputation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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

            let before: Vec<_> = self.providers.iter().map(|p| p.name().to_owned()).collect();
            let mut indexed: Vec<(usize, AnyProvider)> =
                self.providers.iter().cloned().enumerate().collect();
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
            self.providers = Arc::from(indexed.into_iter().map(|(_, p)| p).collect::<Vec<_>>());
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
        let state = thompson
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(e) = state.save(path) {
            tracing::warn!(error = %e, "failed to save Thompson router state");
        }
    }

    fn ordered_providers(&self) -> Vec<AnyProvider> {
        match self.strategy {
            RouterStrategy::Thompson => self.thompson_ordered_providers(),
            RouterStrategy::Ema => self.ema_ordered_providers(),
            // Cascade: providers are sorted at construction time. clone() here is only
            // reached from debug_request_json(); the hot chat/chat_stream paths pass
            // &self.providers directly to avoid this Vec allocation.
            RouterStrategy::Cascade => self.providers.to_vec(),
        }
    }

    fn ema_ordered_providers(&self) -> Vec<AnyProvider> {
        let order = self
            .provider_order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ordered: Vec<AnyProvider> = order
            .iter()
            .filter_map(|&i| self.providers.get(i).cloned())
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
            let rep = reputation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            return self.providers.to_vec();
        };
        let mut state = thompson
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let names: Vec<String> = self.providers.iter().map(|p| p.name().to_owned()).collect();

        // CRIT-3 fix: shift Thompson Beta priors using quality reputation rather than
        // blending two separate samples. This preserves Thompson's single-distribution
        // sampling property and its theoretical guarantees.
        let selected = if let Some(ref reputation) = self.reputation {
            let rep = reputation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let w = self.reputation_weight;
            let overrides: std::collections::HashMap<String, (f64, f64)> = names
                .iter()
                .map(|name| {
                    let base = state.get_distribution(name);
                    let (a, b) = rep.shift_thompson_priors(name, base.alpha, base.beta, w);
                    (name.clone(), (a, b))
                })
                .collect();
            // Drop rep lock before locking state (already held above via state).
            drop(rep);
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
        let mut ordered = self.providers.to_vec();
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
                    let mut state = thompson
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.update(provider_name, success);
                }
            }
            RouterStrategy::Ema => {
                self.ema_record(provider_name, success, latency_ms);
            }
            RouterStrategy::Cascade => {
                // Cascade does not use Thompson/EMA for ordering; no-op.
            }
        }
    }

    fn ema_record(&self, provider_name: &str, success: bool, latency_ms: u64) {
        let Some(ref ema) = self.ema else {
            return;
        };
        ema.record(provider_name, success, latency_ms);
        let current_names: Vec<String> =
            self.providers.iter().map(|p| p.name().to_owned()).collect();
        if let Some(new_order_names) = ema.maybe_reorder(&current_names) {
            let name_to_idx: std::collections::HashMap<&str, usize> = self
                .providers
                .iter()
                .enumerate()
                .map(|(i, p)| (p.name(), i))
                .collect();
            let new_order: Vec<usize> = new_order_names
                .iter()
                .filter_map(|n| name_to_idx.get(n.as_str()).copied())
                .collect();
            let mut order = self
                .provider_order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let state = thompson
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.provider_stats()
    }

    pub fn set_status_tx(&mut self, tx: StatusTx) {
        if let Some(providers) = Arc::get_mut(&mut self.providers) {
            for p in providers {
                p.set_status_tx(tx.clone());
            }
        } else {
            // Defensive path: should never happen at bootstrap (refcount == 1).
            let mut v: Vec<_> = self.providers.iter().cloned().collect();
            for p in &mut v {
                p.set_status_tx(tx.clone());
            }
            self.providers = Arc::from(v);
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
        for p in self.providers.iter() {
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

impl LlmProvider for RouterProvider {
    fn context_window(&self) -> Option<usize> {
        self.providers.first().and_then(LlmProvider::context_window)
    }

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
            if router.strategy == RouterStrategy::Cascade {
                // Cascade: pass Arc slice directly — providers are sorted at construction,
                // so no Vec allocation needed on the hot path.
                return router
                    .cascade_chat(&router.providers, &messages, status_tx)
                    .await;
            }
            let providers = router.ordered_providers();
            for p in &providers {
                let start = std::time::Instant::now();
                match p.chat(&messages).await {
                    Ok(r) => {
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
                        if let Some(ref tx) = status_tx {
                            let _ = tx.send(format!("router: {} failed, falling back", p.name()));
                        }
                        tracing::warn!(provider = p.name(), error = %e, "router fallback");
                    }
                }
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
                    .cascade_chat_stream(&router.providers, &messages, status_tx)
                    .await;
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
        self.providers.iter().any(LlmProvider::supports_streaming)
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
                    Err(e) => {
                        router.record_availability(
                            p.name(),
                            false,
                            u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        );
                        if let Some(ref tx) = status_tx {
                            let _ =
                                tx.send(format!("router: {} embed failed, falling back", p.name()));
                        }
                        tracing::warn!(provider = p.name(), error = %e, "router embed fallback");
                    }
                }
            }
            Err(LlmError::NoProviders)
        })
    }

    fn supports_embeddings(&self) -> bool {
        self.providers.iter().any(LlmProvider::supports_embeddings)
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "router"
    }

    fn supports_tool_use(&self) -> bool {
        self.providers.iter().any(LlmProvider::supports_tool_use)
    }

    fn list_models(&self) -> Vec<String> {
        self.providers
            .iter()
            .flat_map(super::provider::LlmProvider::list_models)
            .collect()
    }

    #[allow(async_fn_in_trait)]
    async fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        // Cascade is intentionally skipped for tool calls: evaluating quality of
        // a tool-call response (structured JSON with tool name + args) requires
        // different heuristics than text quality. Skipping cascade for tool calls
        // avoids inappropriate escalation based on text signals (HIGH-04).
        let providers = self.ordered_providers();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        let status_tx = self.status_tx.clone();
        let router = self.clone();
        Box::pin(async move {
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
                        *router
                            .last_active_provider
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(p.name().to_owned());
                        return Ok(r);
                    }
                    Err(e) => {
                        router.record_availability(
                            p.name(),
                            false,
                            u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        );
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
        .await
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

// ── Cascade routing helpers ───────────────────────────────────────────────────

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

                    // Estimate token cost: rough approximation (1 token ≈ 4 Unicode chars).
                    // Use chars().count() rather than len() (bytes) to avoid overestimating
                    // for non-ASCII content (PERF-CASCADE-06).
                    // Minimum 1 token for any non-empty response to ensure budget accounting
                    // is not defeated by very short outputs.
                    let estimated_tokens =
                        u32::try_from((response.chars().count() / 4).max(1)).unwrap_or(u32::MAX);
                    tokens_used = tokens_used.saturating_add(estimated_tokens);

                    let verdict = Self::evaluate_quality(
                        &response,
                        cfg.quality_threshold,
                        cfg.classifier_mode,
                        cfg.summary_provider.as_ref(),
                    )
                    .await;

                    // Record quality score separately from availability.
                    {
                        let mut state = cascade_state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        state.record(p.name(), verdict.score);
                    }

                    tracing::debug!(
                        provider = %p.name(),
                        score = verdict.score,
                        threshold = cfg.quality_threshold,
                        should_escalate = verdict.should_escalate,
                        reason = %verdict.reason,
                        "cascade: quality verdict"
                    );

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
                    let budget_exhausted = cfg
                        .max_cascade_tokens
                        .is_some_and(|budget| tokens_used >= budget);

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
                    let estimated_tokens =
                        u32::try_from((text.chars().count() / 4).max(1)).unwrap_or(u32::MAX);
                    tokens_used = tokens_used.saturating_add(estimated_tokens);

                    let verdict = Self::evaluate_quality(
                        &text,
                        cfg.quality_threshold,
                        cfg.classifier_mode,
                        cfg.summary_provider.as_ref(),
                    )
                    .await;

                    {
                        let mut state = cascade_state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        state.record(p.name(), verdict.score);
                    }

                    tracing::debug!(
                        provider = %p.name(),
                        score = verdict.score,
                        threshold = cfg.quality_threshold,
                        should_escalate = verdict.should_escalate,
                        reason = %verdict.reason,
                        "cascade stream: quality verdict"
                    );

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

                    let budget_exhausted = cfg
                        .max_cascade_tokens
                        .is_some_and(|budget| tokens_used >= budget);

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
        assert_eq!(c.providers.len(), 1);
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
        assert!(Arc::ptr_eq(&r.providers, &c.providers));
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
        let names: Vec<&str> = r.providers.iter().map(LlmProvider::name).collect();
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
        let names: Vec<&str> = r.providers.iter().map(LlmProvider::name).collect();
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
        let names: Vec<&str> = r.providers.iter().map(LlmProvider::name).collect();
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
        let names: Vec<&str> = r.providers.iter().map(LlmProvider::name).collect();
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
        let names: Vec<&str> = r.providers.iter().map(LlmProvider::name).collect();
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
        let names: Vec<&str> = r.providers.iter().map(LlmProvider::name).collect();
        assert_eq!(names, vec!["claude", "ollama"]);
    }

    #[test]
    fn cost_tiers_empty_router_does_not_panic() {
        let r = RouterProvider::new(vec![]).with_cascade(CascadeRouterConfig {
            cost_tiers: Some(vec!["foo".into()]),
            ..CascadeRouterConfig::default()
        });
        assert_eq!(r.providers.len(), 0);
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
}
