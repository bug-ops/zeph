// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Provider selection and ordering for [`RouterProvider`].
//!
//! Implements the per-strategy ordering (`ema_ordered_providers`,
//! `thompson_ordered_providers`), bandit feature extraction and selection, availability
//! recording, cascade quality evaluation, and the per-turn embedding cache plus ASI
//! coherence update spawning.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use parking_lot::Mutex;

use super::asi::AsiState;
use super::bandit::embedding_to_features;
use super::cascade::{self, ClassifierMode, heuristic_score};
use super::embed_cache::TurnEmbedCache;
use super::{ASI_WARN_LAST_SECS, MAX_ASI_TASKS, RouterProvider, RouterStrategy};
use crate::any::AnyProvider;
use crate::ema::EmaTracker;
use crate::provider::LlmProvider;

impl RouterProvider {
    /// Emit a rate-limited warn (once per 60 s) when a provider's ASI coherence drops below
    /// threshold. Falls back to a trace-level message while the rate limit is active.
    fn maybe_warn_asi_coherence(provider: &str, coherence: f32, threshold: f32) {
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
                provider,
                coherence,
                threshold,
                "asi: coherence below threshold"
            );
        } else {
            tracing::trace!(
                provider,
                coherence,
                threshold,
                "asi: coherence below threshold (warn rate-limited)"
            );
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
    #[tracing::instrument(name = "llm.router.bandit_features", skip_all)]
    pub(crate) async fn bandit_features(&self, query: &str) -> Option<Vec<f32>> {
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
    #[tracing::instrument(name = "llm.router.bandit_select_provider", skip_all)]
    pub(crate) async fn bandit_select_provider(&self, query: &str) -> Option<AnyProvider> {
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
            let raw = self
                .state
                .last_memory_confidence
                .load(std::sync::atomic::Ordering::Relaxed);
            let memory_confidence = if raw == u32::MAX {
                None
            } else {
                Some(f32::from_bits(raw))
            };
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
    pub(crate) fn bandit_record_reward(
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

    pub(crate) fn ordered_providers(&self) -> Vec<AnyProvider> {
        match self.strategy {
            RouterStrategy::Thompson => self.thompson_ordered_providers(),
            RouterStrategy::Ema => self.ema_ordered_providers(),
            // Cascade/Bandit: sync path used only for debug_request_json(); hot paths use
            // dedicated async selection methods. For Cascade, providers are sorted at
            // construction time.
            RouterStrategy::Cascade | RouterStrategy::Bandit => self.state.providers.to_vec(),
        }
    }

    /// Candidate providers for `embed`/`embed_batch`, with the dedicated `embed = true`
    /// provider (if configured via [`crate::router::RouterProvider::with_embed_provider`])
    /// moved to the front.
    ///
    /// This keeps the existing `supports_embeddings()` fallback loop intact while ensuring
    /// a provider explicitly configured for embeddings is tried before any provider that
    /// merely reports `supports_embeddings() == true` (#5859).
    pub(crate) fn embed_candidates(&self) -> Vec<AnyProvider> {
        let mut providers = self.ordered_providers();
        if let Some(dedicated) = self.state.dedicated_embed_provider.as_deref() {
            providers.retain(|p| p.name() != dedicated.name());
            providers.insert(0, dedicated.clone());
        }
        providers
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
                        Self::maybe_warn_asi_coherence(
                            p.name(),
                            coherence,
                            asi_cfg.coherence_threshold,
                        );
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
                            Self::maybe_warn_asi_coherence(
                                name.as_str(),
                                coherence,
                                asi_cfg.coherence_threshold,
                            );
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
    pub(crate) fn record_availability(&self, provider_name: &str, success: bool, latency_ms: u64) {
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
    /// Evaluate quality with heuristics only.
    pub(crate) fn evaluate_heuristic(response: &str, threshold: f64) -> cascade::QualityVerdict {
        let mut verdict = heuristic_score(response);
        verdict.should_escalate = verdict.score < threshold;
        verdict
    }

    /// Evaluate quality using the configured classifier mode.
    ///
    /// For `ClassifierMode::Judge`, calls the summary provider and falls back to heuristic
    /// on any error or timeout. For `ClassifierMode::Heuristic`, evaluates synchronously.
    #[tracing::instrument(name = "llm.router.evaluate_quality", skip_all)]
    pub(crate) async fn evaluate_quality(
        response: &str,
        threshold: f64,
        mode: ClassifierMode,
        summary_provider: Option<&dyn crate::provider_dyn::LlmProviderDyn>,
        judge_timeout_ms: u64,
    ) -> cascade::QualityVerdict {
        if mode == ClassifierMode::Judge {
            if let Some(judge) = summary_provider {
                match cascade::judge_score(
                    judge,
                    response,
                    std::time::Duration::from_millis(judge_timeout_ms),
                )
                .await
                {
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
    /// Embed `text` with per-turn caching.
    ///
    /// Checks `cache` before calling the underlying provider. On a cache hit, increments
    /// `embed_cache_hits`; on a miss, embeds via `self.embed()` and populates the cache.
    /// Either way, `embed_call_count` is incremented for observability.
    #[tracing::instrument(name = "llm.router.embed_cached", skip_all)]
    pub(crate) async fn embed_cached(
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
    pub(crate) fn spawn_asi_update(
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

        let mut tasks = self.asi_tasks.lock();
        // Drain finished tasks so completed handles don't count toward the cap.
        while tasks.try_join_next().is_some() {}
        if tasks.len() >= MAX_ASI_TASKS {
            tracing::debug!("asi: task limit reached, skipping coherence update");
            return;
        }

        let asi = Arc::clone(asi_arc);
        let router = self.clone();
        let window_size = asi_cfg.window;
        let provider_name = provider.to_owned();
        let embed_timeout_ms = self.embed_timeout_ms;
        tasks.spawn(async move {
            let emb = if let Some(e) = precomputed_embedding {
                e
            } else {
                let embed_fut = router.embed(&response);
                let embed_result = if embed_timeout_ms > 0 {
                    let timeout = std::time::Duration::from_millis(embed_timeout_ms);
                    if let Ok(r) = tokio::time::timeout(timeout, embed_fut).await {
                        r
                    } else {
                        tracing::debug!(
                            provider = provider_name,
                            timeout_ms = embed_timeout_ms,
                            "asi: embed timed out, skipping coherence update"
                        );
                        return;
                    }
                } else {
                    embed_fut.await
                };
                match embed_result {
                    Ok(e) => e,
                    Err(err) => {
                        tracing::debug!(
                            provider = provider_name,
                            error = %err,
                            "asi: embed failed, skipping coherence update"
                        );
                        return;
                    }
                }
            };
            let mut state = asi.lock();
            state.push_embedding(&provider_name, emb, window_size);
        });
    }
}
