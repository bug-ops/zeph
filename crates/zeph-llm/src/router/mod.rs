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
pub mod thompson;

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::any::AnyProvider;
use crate::ema::EmaTracker;
use crate::error::LlmError;
use crate::provider::{ChatResponse, ChatStream, LlmProvider, Message, StatusTx, ToolDefinition};

use cascade::{CascadeState, ClassifierMode, heuristic_score};
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
        }
    }
}

#[derive(Debug, Clone)]
pub struct RouterProvider {
    providers: Vec<AnyProvider>,
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
}

impl RouterProvider {
    #[must_use]
    pub fn new(providers: Vec<AnyProvider>) -> Self {
        let n = providers.len();
        Self {
            providers,
            status_tx: None,
            ema: None,
            provider_order: Arc::new(Mutex::new((0..n).collect())),
            strategy: RouterStrategy::Ema,
            thompson: None,
            thompson_state_path: None,
            cascade_state: None,
            cascade_config: None,
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

    /// Enable Cascade routing strategy.
    ///
    /// Providers are tried in chain order (cheapest first). Each response is evaluated
    /// by the quality classifier; if it falls below `quality_threshold`, the next
    /// provider is tried. At most `max_escalations` quality-based escalations occur.
    ///
    /// Network/API errors do not count against the escalation budget.
    /// The best response seen so far is returned if all escalations are exhausted.
    #[must_use]
    pub fn with_cascade(mut self, config: CascadeRouterConfig) -> Self {
        self.strategy = RouterStrategy::Cascade;
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
            // Cascade uses chain order directly (cheapest first = position 0).
            RouterStrategy::Cascade => self.providers.clone(),
        }
    }

    fn ema_ordered_providers(&self) -> Vec<AnyProvider> {
        let order = self
            .provider_order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ordered: Vec<AnyProvider> = order
            .iter()
            .filter_map(|&i| self.providers.get(i).cloned())
            .collect();
        if let Some(first) = ordered.first() {
            let latency_ema_ms = self
                .ema
                .as_ref()
                .and_then(|ema| {
                    let snap = ema.snapshot();
                    snap.get(first.name()).map(|s| s.latency_ema_ms)
                })
                .unwrap_or(500.0);
            tracing::debug!(
                provider = %first.name(),
                strategy = "ema",
                latency_ema_ms = latency_ema_ms,
                "selected provider"
            );
        }
        ordered
    }

    fn thompson_ordered_providers(&self) -> Vec<AnyProvider> {
        let Some(ref thompson) = self.thompson else {
            return self.providers.clone();
        };
        let mut state = thompson
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let names: Vec<String> = self.providers.iter().map(|p| p.name().to_owned()).collect();
        let selected = state.select(&names);
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
        let mut ordered = self.providers.clone();
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
        for p in &mut self.providers {
            p.set_status_tx(tx.clone());
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
        for p in &self.providers {
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
                        return cascade::QualityVerdict {
                            score,
                            should_escalate: score < threshold,
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
        let providers = self.ordered_providers();
        let status_tx = self.status_tx.clone();
        let messages = messages.to_vec();
        let router = self.clone();
        // TODO: DRY — `chat` and `chat_stream` share the same fallback loop pattern.
        // Refactor into a shared helper once the API stabilizes.
        Box::pin(async move {
            if router.strategy == RouterStrategy::Cascade {
                return router.cascade_chat(&providers, &messages, status_tx).await;
            }
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
        let providers = self.ordered_providers();
        let status_tx = self.status_tx.clone();
        let messages = messages.to_vec();
        let router = self.clone();
        Box::pin(async move {
            if router.strategy == RouterStrategy::Cascade {
                return router
                    .cascade_chat_stream(&providers, &messages, status_tx)
                    .await;
            }
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

                    // Update best-seen response.
                    let is_better = best
                        .as_ref()
                        .is_none_or(|(_, best_score)| verdict.score > *best_score);
                    if is_better {
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
                        // When budget is exhausted and we would have escalated, return the best
                        // response seen so far rather than the current (possibly lower-quality) one.
                        if budget_exhausted && verdict.should_escalate {
                            let best_response = best.take().map_or(response, |(r, _)| r);
                            tracing::info!(
                                tokens_used,
                                budget = cfg.max_cascade_tokens,
                                "cascade: token budget exhausted, returning best response"
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
    #[allow(clippy::too_many_lines)]
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

        // Try all providers except the last without consuming the escalation budget
        // for errors (only quality failures consume it).
        let (last, early) = providers.split_last().ok_or(LlmError::NoProviders)?;

        for p in early {
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
                        should_escalate = verdict.should_escalate,
                        "cascade stream: quality verdict"
                    );

                    let budget_exhausted = cfg
                        .max_cascade_tokens
                        .is_some_and(|budget| tokens_used >= budget);

                    if !verdict.should_escalate || escalations_remaining == 0 || budget_exhausted {
                        // Accept: replay as a single-chunk stream.
                        self.record_availability(p.name(), true, latency);
                        let stream: ChatStream = Box::pin(tokio_stream::once(Ok(
                            crate::provider::StreamChunk::Content(text),
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
                        "cascade stream: escalating"
                    );
                }
            }
        }

        // Last provider: stream directly without buffering.
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
            | crate::provider::StreamChunk::Compaction(_) => {}
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
            "".into(),
        ));
        let p2 = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:2",
            "b".into(),
            "".into(),
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
}
