// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Strategy-specific chat execution for [`RouterProvider`].
//!
//! Holds the bandit chat path (`bandit_chat`) and the cascade chat paths
//! (`cascade_chat`, `cascade_chat_stream`) together with their helper types
//! ([`CascadeEvalResult`], [`CollectedStream`]) and stream-collection utilities.

use parking_lot::Mutex;

use super::RouterProvider;
use super::cascade::{self, CascadeState, heuristic_score};
use super::config::CascadeRouterConfig;
use crate::any::AnyProvider;
use crate::error::LlmError;
use crate::provider::{ChatStream, LlmProvider, Message, StatusTx};

// ── Bandit routing helpers ────────────────────────────────────────────────────

impl RouterProvider {
    /// Bandit `chat()` implementation: select provider, call, record reward.
    #[tracing::instrument(name = "llm.router.bandit_chat", skip_all)]
    pub(crate) async fn bandit_chat(
        &self,
        messages: &[Message],
        status_tx: Option<StatusTx>,
    ) -> Result<String, LlmError> {
        let query = messages
            .last()
            .map(crate::provider::Message::to_llm_content)
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
#[tracing::instrument(name = "llm.router.cascade.evaluate_response", skip_all)]
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
        cfg.summary_provider.as_deref(),
        cfg.judge_timeout_ms,
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
    #[tracing::instrument(name = "llm.router.cascade_chat", skip_all)]
    #[allow(clippy::too_many_lines)] // cascade loop: per-provider error/ok/budget/escalation branches are tightly coupled — extracting would obscure the control flow
    pub(crate) async fn cascade_chat(
        &self,
        providers: &[AnyProvider],
        messages: &[Message],
        status_tx: Option<StatusTx>,
    ) -> Result<String, LlmError> {
        let cfg = self
            .cascade_config
            .as_ref()
            .ok_or_else(|| LlmError::Other("cascade_config not set".into()))?;
        let cascade_state = self
            .cascade_state
            .as_ref()
            .ok_or_else(|| LlmError::Other("cascade_state not set".into()))?;

        let mut escalations_remaining = cfg.max_escalations;
        let mut best: Option<(String, f64)> = None; // (response, score)
        let mut tokens_used: u32 = 0;
        let mut last_err: Option<LlmError> = None;

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
                    last_err = Some(e);
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
        best.map(|(r, _)| r)
            .ok_or_else(|| last_err.unwrap_or(LlmError::NoProviders))
    }

    /// Cascade `chat_stream`: buffer cheap response, classify, escalate or replay.
    ///
    /// # Streaming latency tradeoff
    ///
    /// The first N-1 providers are fully buffered before classification. If escalation
    /// occurs, the user experiences: cheap model's full response time + expensive model's
    /// TTFT. This is strictly worse than direct routing to the expensive model for
    /// hard queries. Acceptable for v1; see CRIT-01 in critic handoff for alternatives.
    #[tracing::instrument(name = "llm.router.cascade.chat_stream", skip_all)]
    #[allow(clippy::too_many_lines)] // sequential cascade semantics: buffer→classify→escalate
    pub(crate) async fn cascade_chat_stream(
        &self,
        providers: &[AnyProvider],
        messages: &[Message],
        status_tx: Option<StatusTx>,
    ) -> Result<ChatStream, LlmError> {
        let cfg = self
            .cascade_config
            .as_ref()
            .ok_or_else(|| LlmError::Other("cascade_config not set".into()))?;
        let cascade_state = self
            .cascade_state
            .as_ref()
            .ok_or_else(|| LlmError::Other("cascade_state not set".into()))?;

        let mut escalations_remaining = cfg.max_escalations;
        let mut tokens_used: u32 = 0;
        // Tracks the highest-scoring fully-buffered response seen so far.
        // Only populated from the early provider loop; the last provider streams
        // directly without buffering or scoring, so it never updates best_seen.
        let mut best_seen: Option<(CollectedStream, f64)> = None;

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
                Ok(collected) => {
                    let eval = cascade_evaluate_response(
                        p.name(),
                        &collected.content,
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
                    // Skip empty responses (no content and no tool calls) to avoid
                    // returning silent failures on all-fail fallback.
                    let is_better = !collected.is_empty()
                        && best_seen
                            .as_ref()
                            .is_none_or(|(_, best_score)| verdict.score > *best_score);
                    if is_better {
                        tracing::debug!(
                            provider = %p.name(),
                            score = verdict.score,
                            "cascade stream: best_seen updated"
                        );
                        best_seen = Some((collected.clone(), verdict.score));
                    }

                    if !verdict.should_escalate || escalations_remaining == 0 || budget_exhausted {
                        self.record_availability(p.name(), true, latency);

                        // When escalation is blocked (budget exhausted or escalation count
                        // at zero) and the current response would have triggered escalation,
                        // return the best-seen response instead of the current (possibly
                        // lower-quality) one.
                        let response = if verdict.should_escalate
                            && (budget_exhausted || escalations_remaining == 0)
                        {
                            tracing::info!(
                                tokens_used,
                                budget = cfg.max_cascade_tokens,
                                escalations_remaining,
                                "cascade stream: escalation blocked, returning best response"
                            );
                            best_seen.take().map_or(collected, |(r, _)| r)
                        } else {
                            collected
                        };

                        return Ok(response.into_stream());
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
                if let Some((best_collected, _)) = best_seen {
                    tracing::info!(
                        "cascade stream: last provider failed, returning best-seen response"
                    );
                    return Ok(best_collected.into_stream());
                }
                Err(e)
            }
        }
    }
}

/// Maximum bytes buffered per stream in cascade routing (SEC-CASCADE-03).
const CASCADE_STREAM_MAX_BYTES: usize = 1024 * 1024; // 1 MiB

/// All chunks accumulated from a single provider stream, preserving non-Content chunks.
///
/// Keeping all chunk types allows the router to re-emit a buffered response faithfully
/// (including `Thinking`, `ToolUse`, and `Compaction` chunks) instead of silently
/// dropping them when a best-seen response is replayed.
#[derive(Clone, Default, Debug)]
pub(crate) struct CollectedStream {
    pub(crate) content: String,
    thinking: Vec<String>,
    tool_calls: Vec<crate::provider::ToolUseRequest>,
    compaction: Option<String>,
}

impl CollectedStream {
    /// Reconstructs a `ChatStream` that re-emits all accumulated chunks in order.
    fn into_stream(self) -> ChatStream {
        use crate::provider::StreamChunk;
        let mut chunks: Vec<Result<StreamChunk, LlmError>> = Vec::new();
        for t in self.thinking {
            chunks.push(Ok(StreamChunk::Thinking(t)));
        }
        if !self.tool_calls.is_empty() {
            chunks.push(Ok(StreamChunk::ToolUse(self.tool_calls)));
        }
        if let Some(c) = self.compaction {
            chunks.push(Ok(StreamChunk::Compaction(c)));
        }
        if !self.content.is_empty() {
            chunks.push(Ok(StreamChunk::Content(self.content)));
        }
        Box::pin(tokio_stream::iter(chunks))
    }

    fn is_empty(&self) -> bool {
        self.content.is_empty() && self.tool_calls.is_empty()
    }
}

/// Collect a `ChatStream` into a [`CollectedStream`], preserving all chunk types.
///
/// Returns `Err` if the accumulated `Content` buffer exceeds [`CASCADE_STREAM_MAX_BYTES`].
pub(crate) async fn collect_stream(stream: ChatStream) -> Result<CollectedStream, LlmError> {
    use tokio_stream::StreamExt as _;

    let mut stream = stream;
    let mut collected = CollectedStream::default();
    while let Some(chunk) = stream.next().await {
        match chunk? {
            crate::provider::StreamChunk::Content(c) => {
                if collected.content.len() + c.len() > CASCADE_STREAM_MAX_BYTES {
                    return Err(LlmError::Other(
                        "cascade: stream response exceeds 1 MiB buffer limit".into(),
                    ));
                }
                collected.content.push_str(&c);
            }
            crate::provider::StreamChunk::Thinking(t) => {
                collected.thinking.push(t);
            }
            crate::provider::StreamChunk::ToolUse(tools) => {
                collected.tool_calls.extend(tools);
            }
            crate::provider::StreamChunk::Compaction(c) => {
                collected.compaction = Some(c);
            }
        }
    }
    Ok(collected)
}
