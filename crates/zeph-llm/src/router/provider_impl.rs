// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`LlmProvider`] trait implementation for [`RouterProvider`].
//!
//! Forwards `chat`/`chat_stream`/`embed`/`embed_batch` and metadata queries to the
//! selected backend, applying the fallback loop, retry/backoff for embeddings, the
//! quality gate, `CoE` escalation, and ASI coherence tracking.

use std::sync::atomic::Ordering;

use parking_lot::Mutex;

use super::coe::{CoeDecision, run_coe};
use super::embed_cache::TurnEmbedCache;
use super::{RouterProvider, RouterStrategy};
use crate::embed::owned_strs;
use crate::error::LlmError;
use crate::provider::{ChatResponse, ChatStream, LlmProvider, Message, StatusTx, ToolDefinition};
use zeph_common::math::cosine_similarity;

const EMBED_MAX_RETRIES: u32 = 3;
const EMBED_BASE_DELAY_MS: u64 = 500;

/// Record a provider error during the fallback loop and emit a warning log.
///
/// Shared by [`RouterProvider::chat`] and [`RouterProvider::chat_stream`] to avoid
/// duplicating error-path bookkeeping. Not part of the public API.
fn record_fallback_error(
    router: &RouterProvider,
    provider_name: &str,
    error: &LlmError,
    elapsed_ms: u64,
    status_tx: Option<&StatusTx>,
    log_msg: &'static str,
) {
    router.record_availability(provider_name, false, elapsed_ms);
    if error.is_rate_limited() {
        router.record_availability(provider_name, false, 0);
    }
    if let Some(tx) = status_tx {
        let _ = tx.send(format!("router: {provider_name} failed, falling back"));
    }
    tracing::warn!(provider = provider_name, error = %error, "{}", log_msg);
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
        let model = self.model_identifier().to_owned();
        // NOTE: `chat` and `chat_stream` share error-path logic via `record_fallback_error`.
        // Their success paths diverge (quality gate + CoE vs. plain stream-open), so a
        // shared loop helper would reduce clarity without removing significant duplication.
        let fut = Box::pin(async move {
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
            // Preserve the most recent error across the fallback loop so an exhausted
            // loop surfaces the actionable diagnostic instead of a generic `NoProviders`.
            let mut last_err: Option<LlmError> = None;

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
                        record_fallback_error(
                            &router,
                            p.name(),
                            &e,
                            u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                            status_tx.as_ref(),
                            "router fallback",
                        );
                        last_err = Some(e);
                    }
                }
            }

            // All providers exhausted by quality gate: return best response seen (M2).
            if let Some((_, response)) = best_response {
                return Ok(response);
            }

            Err(last_err.unwrap_or(LlmError::NoProviders))
        });
        {
            use tracing::Instrument as _;
            fut.instrument(tracing::info_span!("llm.router.chat", model = model))
        }
    }

    fn chat_stream(
        &self,
        messages: &[Message],
    ) -> impl std::future::Future<Output = Result<ChatStream, LlmError>> + Send {
        let status_tx = self.status_tx.clone();
        let messages = messages.to_vec();
        let router = self.clone();
        let model = self.model_identifier().to_owned();
        let fut = Box::pin(async move {
            // NOTE: see DRY design decision above `chat()` — error path shared via
            // `record_fallback_error`; success paths diverge intentionally.
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
                    .map(crate::provider::Message::to_llm_content)
                    .unwrap_or_default();
                let p = router
                    .bandit_select_provider(query)
                    .await
                    .ok_or(LlmError::NoProviders)?;
                return p.chat_stream(&messages).await;
            }
            let providers = router.ordered_providers();
            // Preserve the most recent error across the fallback loop so an exhausted
            // loop surfaces the actionable diagnostic instead of a generic `NoProviders`.
            let mut last_err: Option<LlmError> = None;
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
                        record_fallback_error(
                            &router,
                            p.name(),
                            &e,
                            u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                            status_tx.as_ref(),
                            "router stream fallback",
                        );
                        last_err = Some(e);
                    }
                }
            }
            Err(last_err.unwrap_or(LlmError::NoProviders))
        });
        {
            use tracing::Instrument as _;
            fut.instrument(tracing::info_span!("llm.router.chat_stream", model = model))
        }
    }

    fn supports_streaming(&self) -> bool {
        self.state
            .providers
            .iter()
            .any(LlmProvider::supports_streaming)
    }

    #[allow(clippy::too_many_lines)] // retry + timeout + fallback + availability tracking: splitting would break the shared `last_err` accumulator
    fn embed(
        &self,
        text: &str,
    ) -> impl std::future::Future<Output = Result<Vec<f32>, LlmError>> + Send {
        let providers = self.ordered_providers();
        let status_tx = self.status_tx.clone();
        let text = text.to_owned();
        let router = self.clone();
        let embed_timeout_ms = self.embed_timeout_ms;
        let model = self.model_identifier().to_owned();
        let fut = Box::pin(async move {
            // Preserve the most recent error across the fallback loop so an exhausted
            // loop surfaces the actionable diagnostic instead of a generic `NoProviders`.
            let mut last_err: Option<LlmError> = None;
            for p in &providers {
                if !p.supports_embeddings() {
                    continue;
                }
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
                    // Apply per-call timeout when configured (embed_timeout_ms > 0).
                    let embed_result: Result<Vec<f32>, LlmError> = if embed_timeout_ms > 0 {
                        let timeout = std::time::Duration::from_millis(embed_timeout_ms);
                        match tokio::time::timeout(timeout, p.embed(&text)).await {
                            Ok(inner) => inner,
                            Err(_elapsed) => {
                                tracing::warn!(
                                    provider = p.name(),
                                    timeout_ms = embed_timeout_ms,
                                    "embed: provider timed out, falling back"
                                );
                                router.record_availability(
                                    p.name(),
                                    false,
                                    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                                );
                                last_err = Some(LlmError::Timeout);
                                break;
                            }
                        }
                    } else {
                        p.embed(&text).await
                    };
                    match embed_result {
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
            Err(last_err.unwrap_or(LlmError::NoProviders))
        });
        {
            use tracing::Instrument as _;
            fut.instrument(tracing::info_span!("llm.router.embed", model = model))
        }
    }

    #[allow(clippy::too_many_lines)] // retry + timeout + fallback + availability tracking: splitting would break the shared `last_err` accumulator
    fn embed_batch(
        &self,
        texts: &[&str],
    ) -> impl std::future::Future<Output = Result<Vec<Vec<f32>>, LlmError>> + Send {
        let providers = self.ordered_providers();
        let status_tx = self.status_tx.clone();
        let owned = owned_strs(texts);
        let router = self.clone();
        let semaphore = self.state.embed_semaphore.clone();
        let embed_timeout_ms = self.embed_timeout_ms;
        let model = self.model_identifier().to_owned();
        let fut = Box::pin(async move {
            // Acquire embed semaphore permit before any HTTP work to cap concurrency.
            let _permit = if let Some(ref sem) = semaphore {
                Some(sem.acquire().await.map_err(|_| LlmError::NoProviders)?)
            } else {
                None
            };
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            // Preserve the most recent error across the fallback loop so an exhausted
            // loop surfaces the actionable diagnostic instead of a generic `NoProviders`.
            let mut last_err: Option<LlmError> = None;
            for p in &providers {
                if !p.supports_embeddings() {
                    continue;
                }
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
                    // Apply per-call timeout when configured (embed_timeout_ms > 0).
                    let embed_result: Result<Vec<Vec<f32>>, LlmError> = if embed_timeout_ms > 0 {
                        let timeout = std::time::Duration::from_millis(embed_timeout_ms);
                        match tokio::time::timeout(timeout, p.embed_batch(&refs)).await {
                            Ok(inner) => inner,
                            Err(_elapsed) => {
                                tracing::warn!(
                                    provider = p.name(),
                                    timeout_ms = embed_timeout_ms,
                                    "embed_batch: provider timed out, falling back"
                                );
                                router.record_availability(
                                    p.name(),
                                    false,
                                    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                                );
                                last_err = Some(LlmError::Timeout);
                                break;
                            }
                        }
                    } else {
                        p.embed_batch(&refs).await
                    };
                    match embed_result {
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
            Err(last_err.unwrap_or(LlmError::NoProviders))
        });
        {
            use tracing::Instrument as _;
            fut.instrument(tracing::info_span!("llm.router.embed_batch", model = model))
        }
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

    #[allow(clippy::unnecessary_literal_bound)]
    fn model_identifier(&self) -> &str {
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
            .flat_map(crate::provider::LlmProvider::list_models)
            .collect()
    }

    #[allow(refining_impl_trait_reachable)]
    fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> impl std::future::Future<Output = Result<ChatResponse, LlmError>> + Send {
        let messages = messages.to_vec();
        let tool_count = tools.len();
        let tools = tools.to_vec();
        let status_tx = self.status_tx.clone();
        let router = self.clone();
        let model = self.model_identifier().to_owned();
        let fut = Box::pin(async move {
            // Bandit routing for tool calls: select a single provider, no quality escalation.
            if router.strategy == RouterStrategy::Bandit {
                let query = messages
                    .last()
                    .map(crate::provider::Message::to_llm_content)
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
            // Preserve the most recent error across the fallback loop so an exhausted
            // loop surfaces the actionable diagnostic (e.g. `ModelCapabilityMismatch`'s
            // enriched message from #5795) instead of a generic `NoProviders`.
            let mut last_err: Option<LlmError> = None;
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
                        // A model capability mismatch (e.g. `reasoning_effort` + `tools` on
                        // this specific model/provider) is retryable elsewhere — the same
                        // request may succeed on a different model or provider, unlike
                        // `InvalidInput` which is malformed regardless of destination.
                        if e.is_model_capability_mismatch() {
                            tracing::warn!(
                                provider = p.name(),
                                error = %e,
                                "chat_with_tools: model capability mismatch, falling back to next provider"
                            );
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
                        last_err = Some(e);
                    }
                }
            }
            Err(last_err.unwrap_or(LlmError::NoProviders))
        });
        {
            use tracing::Instrument as _;
            fut.instrument(tracing::info_span!(
                "llm.router.chat_with_tools",
                model = model,
                tool_count = tool_count
            ))
        }
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
                .find(crate::provider::LlmProvider::supports_tool_use)
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
