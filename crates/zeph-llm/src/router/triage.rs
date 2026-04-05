// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Complexity triage routing: pre-inference classification that selects the optimal
//! provider tier based on input difficulty.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use zeph_common::text::truncate_chars;

use crate::any::AnyProvider;
use crate::embed::owned_strs;
use crate::error::LlmError;
use crate::provider::{
    ChatResponse, ChatStream, LlmProvider, Message, MessageMetadata, Role, StatusTx, ToolDefinition,
};

/// Complexity tier for input classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ComplexityTier {
    #[default]
    Simple,
    Medium,
    Complex,
    Expert,
}

impl ComplexityTier {
    /// Returns the display name for this tier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Medium => "medium",
            Self::Complex => "complex",
            Self::Expert => "expert",
        }
    }

    /// Returns the ordered index (0 = cheapest/simplest).
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            Self::Simple => 0,
            Self::Medium => 1,
            Self::Complex => 2,
            Self::Expert => 3,
        }
    }

    /// Returns tiers in ascending order (Simple -> Expert).
    #[must_use]
    pub fn ascending() -> [Self; 4] {
        [Self::Simple, Self::Medium, Self::Complex, Self::Expert]
    }
}

/// Result of triage classification.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TriageVerdict {
    pub tier: ComplexityTier,
    pub reason: String,
    #[serde(default)]
    pub large_context: bool,
}

/// Metrics counters for triage routing. All counters use `AtomicU64` for lock-free async access.
#[derive(Debug, Default)]
pub struct TriageMetrics {
    pub calls: AtomicU64,
    pub tier_simple: AtomicU64,
    pub tier_medium: AtomicU64,
    pub tier_complex: AtomicU64,
    pub tier_expert: AtomicU64,
    /// Fallbacks due to triage timeout or parse failure.
    pub timeout_fallbacks: AtomicU64,
    /// Context window auto-escalations.
    pub escalations: AtomicU64,
    /// Triage call latency accumulator (microseconds, for averaging).
    pub latency_us_total: AtomicU64,
}

impl TriageMetrics {
    fn record_tier(&self, tier: ComplexityTier) {
        match tier {
            ComplexityTier::Simple => self.tier_simple.fetch_add(1, Ordering::Relaxed),
            ComplexityTier::Medium => self.tier_medium.fetch_add(1, Ordering::Relaxed),
            ComplexityTier::Complex => self.tier_complex.fetch_add(1, Ordering::Relaxed),
            ComplexityTier::Expert => self.tier_expert.fetch_add(1, Ordering::Relaxed),
        };
    }

    /// Returns a snapshot: (simple, medium, complex, expert, fallbacks, escalations).
    #[must_use]
    pub fn snapshot(&self) -> (u64, u64, u64, u64, u64, u64) {
        (
            self.tier_simple.load(Ordering::Relaxed),
            self.tier_medium.load(Ordering::Relaxed),
            self.tier_complex.load(Ordering::Relaxed),
            self.tier_expert.load(Ordering::Relaxed),
            self.timeout_fallbacks.load(Ordering::Relaxed),
            self.escalations.load(Ordering::Relaxed),
        )
    }

    /// Average latency in microseconds (0 if no calls).
    #[must_use]
    pub fn avg_latency_us(&self) -> u64 {
        let calls = self.calls.load(Ordering::Relaxed);
        if calls == 0 {
            return 0;
        }
        self.latency_us_total.load(Ordering::Relaxed) / calls
    }
}

/// Sentinel for `last_provider_idx` meaning "no request completed yet".
const NO_LAST_PROVIDER: usize = usize::MAX;

/// Pre-inference complexity router. Classifies each request with a cheap triage model,
/// then delegates to the appropriate tier provider.
#[derive(Clone)]
pub struct TriageRouter {
    /// Cheap/fast model for classification.
    triage_provider: AnyProvider,
    /// Ordered list: (tier, provider). Simple first, Expert last.
    tier_providers: Vec<(ComplexityTier, AnyProvider)>,
    /// Index into `tier_providers` used when triage fails.
    default_index: usize,
    /// Triage call timeout.
    triage_timeout: Duration,
    // Reserved for future use: max_triage_tokens controls triage model output budget.
    _max_triage_tokens: u32,
    /// Metrics counters.
    metrics: Arc<TriageMetrics>,
    /// Index of the last-used tier provider (for token usage delegation).
    /// Shared via `Arc` so `Clone` copies see the same last-used state.
    /// Value `NO_LAST_PROVIDER` means no request has completed yet.
    last_provider_idx: Arc<AtomicUsize>,
    /// Router display name.
    name: String,
}

impl std::fmt::Debug for TriageRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TriageRouter")
            .field("name", &self.name)
            .field(
                "tiers",
                &self
                    .tier_providers
                    .iter()
                    .map(|(t, _)| t.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl TriageRouter {
    /// Create a new `TriageRouter`.
    ///
    /// # Panics
    ///
    /// Panics if `tier_providers` is empty.
    #[must_use]
    pub fn new(
        triage_provider: AnyProvider,
        tier_providers: Vec<(ComplexityTier, AnyProvider)>,
        triage_timeout_secs: u64,
        max_triage_tokens: u32,
    ) -> Self {
        assert!(
            !tier_providers.is_empty(),
            "TriageRouter requires at least one tier provider"
        );
        // Default: first in list (lowest tier / cheapest).
        Self {
            triage_provider,
            tier_providers,
            default_index: 0,
            triage_timeout: Duration::from_secs(triage_timeout_secs),
            _max_triage_tokens: max_triage_tokens,
            metrics: Arc::new(TriageMetrics::default()),
            last_provider_idx: Arc::new(AtomicUsize::new(NO_LAST_PROVIDER)),
            name: "triage".to_owned(),
        }
    }

    /// Propagate a status sender to all tier providers.
    pub fn set_status_tx(&mut self, tx: &StatusTx) {
        for (_, provider) in &mut self.tier_providers {
            provider.set_status_tx(tx.clone());
        }
    }

    /// Returns a reference to the metrics.
    #[must_use]
    pub fn metrics(&self) -> &Arc<TriageMetrics> {
        &self.metrics
    }

    /// Classify the last user message and return the selected provider index into `tier_providers`.
    /// On failure (timeout, parse error), returns `default_index`.
    async fn classify(&self, messages: &[Message]) -> usize {
        let start = std::time::Instant::now();
        self.metrics.calls.fetch_add(1, Ordering::Relaxed);

        let result = self.try_classify(messages).await;

        let elapsed = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.metrics
            .latency_us_total
            .fetch_add(elapsed, Ordering::Relaxed);

        if let Some(tier) = result {
            self.metrics.record_tier(tier);
            self.select_provider_for_tier(tier)
        } else {
            self.metrics
                .timeout_fallbacks
                .fetch_add(1, Ordering::Relaxed);
            tracing::warn!("triage classification failed, falling back to default provider");
            self.default_index
        }
    }

    async fn try_classify(&self, messages: &[Message]) -> Option<ComplexityTier> {
        let prompt = build_triage_prompt(messages);
        let triage_msg = Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        };

        let triage_result = tokio::time::timeout(
            self.triage_timeout,
            self.triage_provider.chat(&[triage_msg]),
        )
        .await;

        let raw = match triage_result {
            Ok(Ok(text)) => text,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "triage provider returned error");
                return None;
            }
            Err(_elapsed) => {
                tracing::warn!(
                    timeout_secs = self.triage_timeout.as_secs(),
                    "triage call timed out"
                );
                return None;
            }
        };

        parse_tier_from_response(&raw)
    }

    /// Find the best provider index for the given tier, with fallback escalation.
    /// If the exact tier is not present, escalate to the next higher tier.
    fn select_provider_for_tier(&self, tier: ComplexityTier) -> usize {
        // Try exact match first.
        if let Some(idx) = self.tier_providers.iter().position(|(t, _)| *t == tier) {
            return idx;
        }
        // Escalate: try higher tiers in ascending order.
        for candidate in ComplexityTier::ascending() {
            if candidate.index() > tier.index()
                && let Some(idx) = self
                    .tier_providers
                    .iter()
                    .position(|(t, _)| *t == candidate)
            {
                tracing::debug!(
                    tier = tier.as_str(),
                    escalated_to = candidate.as_str(),
                    "triage: tier not configured, escalating"
                );
                return idx;
            }
        }
        // Descend if no higher tier found.
        for candidate in ComplexityTier::ascending().into_iter().rev() {
            if candidate.index() < tier.index()
                && let Some(idx) = self
                    .tier_providers
                    .iter()
                    .position(|(t, _)| *t == candidate)
            {
                return idx;
            }
        }
        self.default_index
    }

    /// Apply D6 context window check: if context tokens exceed 80% of the selected
    /// provider's window, escalate to the smallest provider whose window fits.
    /// When `context_window()` returns `None`, skip the check (MF-3).
    fn maybe_escalate_for_context(&self, idx: usize, context_tokens: usize) -> usize {
        let Some(window) = self.tier_providers[idx].1.context_window() else {
            return idx;
        };
        if context_tokens <= window * 4 / 5 {
            return idx;
        }
        // Current window too small; find the smallest provider that fits.
        let mut best = idx;
        for (i, (_, provider)) in self.tier_providers.iter().enumerate() {
            if let Some(w) = provider.context_window()
                && w > window
                && context_tokens <= w * 4 / 5
            {
                best = i;
                break; // tier_providers is ordered smallest→largest; first fit wins.
            }
        }
        if best != idx {
            self.metrics.escalations.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                original_tier = self.tier_providers[idx].0.as_str(),
                escalated_tier = self.tier_providers[best].0.as_str(),
                context_tokens,
                "triage: auto-escalated due to context window overflow"
            );
        }
        best
    }
}

fn build_triage_prompt(messages: &[Message]) -> String {
    let last_user = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map_or("", |m| m.content.as_str());
    // Truncate to keep triage cost minimal (~120 input tokens).
    let truncated = truncate_chars(last_user, 400);

    format!(
        r#"Classify the complexity of the following user request. Consider:
- Number of reasoning steps required
- Domain expertise needed
- Whether the task is well-defined or open-ended

Tiers:
- simple: greeting, factual lookup, yes/no, single-step task
- medium: multi-step but well-defined, moderate reasoning
- complex: deep analysis, multi-turn planning, creative synthesis, debugging
- expert: domain expertise, long-form generation, system architecture, research

User message:
{truncated}

Respond ONLY with JSON: {{"tier":"simple|medium|complex|expert","reason":"...","large_context":false}}"#
    )
}

fn parse_tier_from_response(raw: &str) -> Option<ComplexityTier> {
    // Try JSON parse first.
    if let Ok(verdict) = serde_json::from_str::<TriageVerdict>(raw) {
        return Some(verdict.tier);
    }
    // Try extracting from partial/embedded JSON.
    let trimmed = raw.trim();
    if let Some(start) = trimmed.find('{')
        && let Some(end) = trimmed.rfind('}')
    {
        let json_fragment = &trimmed[start..=end];
        if let Ok(verdict) = serde_json::from_str::<TriageVerdict>(json_fragment) {
            return Some(verdict.tier);
        }
    }
    // Substring fallback: look for tier value patterns in the raw text.
    for (needle, tier) in [
        ("\"simple\"", ComplexityTier::Simple),
        ("\"medium\"", ComplexityTier::Medium),
        ("\"complex\"", ComplexityTier::Complex),
        ("\"expert\"", ComplexityTier::Expert),
    ] {
        // Only match when preceded by "tier" key to avoid false positives.
        if let Some(tier_pos) = raw.find("\"tier\"") {
            let after_key = &raw[tier_pos..];
            if after_key.contains(needle) {
                return Some(tier);
            }
        } else if raw.contains(needle) {
            return Some(tier);
        }
    }
    None
}

impl LlmProvider for TriageRouter {
    fn name(&self) -> &str {
        &self.name
    }

    fn context_window(&self) -> Option<usize> {
        // Return the largest context window across all tier providers.
        self.tier_providers
            .iter()
            .filter_map(|(_, p)| p.context_window())
            .max()
    }

    fn supports_streaming(&self) -> bool {
        self.tier_providers
            .iter()
            .any(|(_, p)| p.supports_streaming())
    }

    fn supports_embeddings(&self) -> bool {
        self.tier_providers
            .iter()
            .any(|(_, p)| p.supports_embeddings())
            || self.triage_provider.supports_embeddings()
    }

    fn supports_structured_output(&self) -> bool {
        false
    }

    fn supports_vision(&self) -> bool {
        self.tier_providers.iter().any(|(_, p)| p.supports_vision())
    }

    fn supports_tool_use(&self) -> bool {
        self.tier_providers
            .iter()
            .any(|(_, p)| p.supports_tool_use())
    }

    fn embed(
        &self,
        text: &str,
    ) -> impl std::future::Future<Output = Result<Vec<f32>, LlmError>> + Send {
        // Delegate to the first embedding-capable tier provider, then to the triage provider,
        // so that tool schema filter initialization works when routing = "triage".
        let embed_provider = self
            .tier_providers
            .iter()
            .find(|(_, p)| p.supports_embeddings())
            .map(|(_, p)| p.clone())
            .or_else(|| {
                self.triage_provider
                    .supports_embeddings()
                    .then(|| self.triage_provider.clone())
            });

        let name = self.name.clone();
        let text = text.to_owned();
        Box::pin(async move {
            match embed_provider {
                Some(p) => p.embed(&text).await,
                None => Err(LlmError::EmbedUnsupported { provider: name }),
            }
        })
    }

    fn embed_batch(
        &self,
        texts: &[&str],
    ) -> impl std::future::Future<Output = Result<Vec<Vec<f32>>, LlmError>> + Send {
        let embed_provider = self
            .tier_providers
            .iter()
            .find(|(_, p)| p.supports_embeddings())
            .map(|(_, p)| p.clone())
            .or_else(|| {
                self.triage_provider
                    .supports_embeddings()
                    .then(|| self.triage_provider.clone())
            });

        let name = self.name.clone();
        let owned = owned_strs(texts);
        Box::pin(async move {
            match embed_provider {
                Some(p) => {
                    let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
                    p.embed_batch(&refs).await
                }
                None => Err(LlmError::EmbedUnsupported { provider: name }),
            }
        })
    }

    /// Classify + delegate: each method independently performs triage (MF-2).
    #[allow(refining_impl_trait_reachable)]
    fn chat(
        &self,
        messages: &[Message],
    ) -> impl std::future::Future<Output = Result<String, LlmError>> + Send {
        let router = self.clone();
        let messages = messages.to_vec();
        Box::pin(async move {
            let context_tokens: usize = messages.iter().map(|m| m.content.len() / 4).sum();
            let idx = router.classify(&messages).await;
            let idx = router.maybe_escalate_for_context(idx, context_tokens);
            let (tier, provider) = &router.tier_providers[idx];
            tracing::debug!(
                tier = tier.as_str(),
                provider = provider.name(),
                "triage routing: chat"
            );
            let result = provider.chat(&messages).await;
            router.last_provider_idx.store(idx, Ordering::Relaxed);
            result
        })
    }

    /// Classify + delegate: each method independently performs triage (MF-2).
    #[allow(refining_impl_trait_reachable)]
    fn chat_stream(
        &self,
        messages: &[Message],
    ) -> impl std::future::Future<Output = Result<ChatStream, LlmError>> + Send {
        let router = self.clone();
        let messages = messages.to_vec();
        Box::pin(async move {
            let context_tokens: usize = messages.iter().map(|m| m.content.len() / 4).sum();
            let idx = router.classify(&messages).await;
            let idx = router.maybe_escalate_for_context(idx, context_tokens);
            let (tier, provider) = &router.tier_providers[idx];
            tracing::debug!(
                tier = tier.as_str(),
                provider = provider.name(),
                "triage routing: chat_stream"
            );
            let result = provider.chat_stream(&messages).await;
            router.last_provider_idx.store(idx, Ordering::Relaxed);
            result
        })
    }

    /// Classify + delegate: each method independently performs triage (MF-2).
    #[allow(refining_impl_trait_reachable)]
    fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> impl std::future::Future<Output = Result<ChatResponse, LlmError>> + Send {
        let router = self.clone();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        Box::pin(async move {
            let context_tokens: usize = messages.iter().map(|m| m.content.len() / 4).sum();
            let idx = router.classify(&messages).await;
            let idx = router.maybe_escalate_for_context(idx, context_tokens);
            let (tier, provider) = &router.tier_providers[idx];
            tracing::debug!(
                tier = tier.as_str(),
                provider = provider.name(),
                "triage routing: chat_with_tools"
            );
            let result = provider.chat_with_tools(&messages, &tools).await;
            router.last_provider_idx.store(idx, Ordering::Relaxed);
            result
        })
    }

    fn last_usage(&self) -> Option<(u64, u64)> {
        let idx = self.last_provider_idx.load(Ordering::Relaxed);
        if idx == NO_LAST_PROVIDER {
            return None;
        }
        self.tier_providers
            .get(idx)
            .and_then(|(_, p)| p.last_usage())
    }

    fn last_cache_usage(&self) -> Option<(u64, u64)> {
        let idx = self.last_provider_idx.load(Ordering::Relaxed);
        if idx == NO_LAST_PROVIDER {
            return None;
        }
        self.tier_providers
            .get(idx)
            .and_then(|(_, p)| p.last_cache_usage())
    }

    fn debug_request_json(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream: bool,
    ) -> serde_json::Value {
        // Use the last-selected tier provider when available; fall back to the first tier.
        let idx = self.last_provider_idx.load(Ordering::Relaxed);
        let provider = if idx == NO_LAST_PROVIDER {
            self.tier_providers.first().map(|(_, p)| p)
        } else {
            self.tier_providers.get(idx).map(|(_, p)| p)
        };
        provider.map_or(serde_json::Value::Null, |p| {
            p.debug_request_json(messages, tools, stream)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockProvider;

    fn mock_provider(name: &str) -> AnyProvider {
        let mut p = MockProvider::default();
        p.name_override = Some(name.to_owned());
        AnyProvider::Mock(p)
    }

    fn triage_mock(response: &str) -> AnyProvider {
        AnyProvider::Mock(MockProvider::with_responses(vec![response.to_owned()]))
    }

    fn make_user_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: text.to_owned(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }
    }

    #[test]
    fn parse_tier_json() {
        let raw = r#"{"tier":"simple","reason":"greeting","large_context":false}"#;
        assert_eq!(parse_tier_from_response(raw), Some(ComplexityTier::Simple));
    }

    #[test]
    fn parse_tier_complex() {
        let raw = r#"{"tier":"complex","reason":"deep analysis"}"#;
        assert_eq!(parse_tier_from_response(raw), Some(ComplexityTier::Complex));
    }

    #[test]
    fn parse_tier_regex_fallback() {
        let raw = r#"here is the json: {"tier": "expert","reason":"architecture"}"#;
        assert_eq!(parse_tier_from_response(raw), Some(ComplexityTier::Expert));
    }

    #[test]
    fn parse_tier_regex_only() {
        // Only regex can extract — no JSON braces
        let raw = r#"the tier is "tier": "medium" I think"#;
        assert_eq!(parse_tier_from_response(raw), Some(ComplexityTier::Medium));
    }

    #[test]
    fn parse_tier_garbage_returns_none() {
        assert_eq!(parse_tier_from_response("hello world"), None);
    }

    #[test]
    fn select_provider_exact_tier() {
        let router = TriageRouter::new(
            triage_mock(r#"{"tier":"medium"}"#),
            vec![
                (ComplexityTier::Simple, mock_provider("simple-p")),
                (ComplexityTier::Medium, mock_provider("medium-p")),
            ],
            5,
            50,
        );
        let idx = router.select_provider_for_tier(ComplexityTier::Medium);
        assert_eq!(idx, 1);
    }

    #[test]
    fn select_provider_escalates_to_higher() {
        // Simple tier not configured; should escalate to medium.
        let router = TriageRouter::new(
            triage_mock(r#"{"tier":"simple"}"#),
            vec![
                (ComplexityTier::Medium, mock_provider("medium-p")),
                (ComplexityTier::Expert, mock_provider("expert-p")),
            ],
            5,
            50,
        );
        let idx = router.select_provider_for_tier(ComplexityTier::Simple);
        assert_eq!(idx, 0); // medium-p is the lowest available
    }

    #[test]
    fn complexity_tier_index_ordering() {
        assert!(ComplexityTier::Simple.index() < ComplexityTier::Medium.index());
        assert!(ComplexityTier::Medium.index() < ComplexityTier::Complex.index());
        assert!(ComplexityTier::Complex.index() < ComplexityTier::Expert.index());
    }

    #[test]
    fn build_triage_prompt_contains_last_user_message() {
        let messages = vec![
            Message {
                role: Role::System,
                content: "You are helpful".to_owned(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            make_user_msg("explain quantum entanglement"),
        ];
        let prompt = build_triage_prompt(&messages);
        assert!(prompt.contains("explain quantum entanglement"));
        assert!(prompt.contains("simple|medium|complex|expert"));
    }

    #[test]
    fn tier_as_str() {
        assert_eq!(ComplexityTier::Simple.as_str(), "simple");
        assert_eq!(ComplexityTier::Expert.as_str(), "expert");
    }

    #[tokio::test]
    async fn triage_router_chat_delegates_to_correct_tier() {
        let simple_response = "simple answer";
        let router = TriageRouter::new(
            triage_mock(r#"{"tier":"simple","reason":"greeting"}"#),
            vec![
                (
                    ComplexityTier::Simple,
                    AnyProvider::Mock(MockProvider::with_responses(vec![
                        simple_response.to_owned(),
                    ])),
                ),
                (
                    ComplexityTier::Complex,
                    AnyProvider::Mock(MockProvider::with_responses(vec![
                        "complex answer".to_owned(),
                    ])),
                ),
            ],
            5,
            50,
        );
        let messages = vec![make_user_msg("hi")];
        let result = router.chat(&messages).await.unwrap();
        assert_eq!(result, simple_response);
    }

    #[tokio::test]
    async fn triage_router_fallback_on_timeout() {
        // Triage provider that sleeps for 60 seconds → timeout fallback to default (index 0).
        let slow_triage = AnyProvider::Mock(MockProvider::default().with_delay(60_000)); // 60 000 ms
        let router = TriageRouter::new(
            slow_triage,
            vec![(
                ComplexityTier::Simple,
                AnyProvider::Mock(MockProvider::with_responses(vec![
                    "default-answer".to_owned(),
                ])),
            )],
            1, // 1 second timeout
            50,
        );
        let messages = vec![make_user_msg("test")];
        let result = router.chat(&messages).await.unwrap();
        assert_eq!(result, "default-answer");
        assert_eq!(router.metrics.timeout_fallbacks.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn metrics_snapshot_default() {
        let m = TriageMetrics::default();
        let snap = m.snapshot();
        assert_eq!(snap, (0, 0, 0, 0, 0, 0));
    }

    // ── maybe_escalate_for_context tests ─────────────────────────────────────

    fn mock_no_window() -> AnyProvider {
        AnyProvider::Mock(MockProvider::default())
    }

    fn ollama_with_window(context_window: usize) -> AnyProvider {
        let mut p = crate::ollama::OllamaProvider::new(
            "http://localhost:11434",
            "llama3".to_owned(),
            String::new(),
        );
        p.set_context_window(context_window);
        AnyProvider::Ollama(p)
    }

    #[test]
    fn escalate_skips_when_context_window_none() {
        // Provider with no context_window (MockProvider default) → escalation must be skipped.
        let router = TriageRouter::new(
            triage_mock(r#"{"tier":"simple"}"#),
            vec![(ComplexityTier::Simple, mock_no_window())],
            5,
            50,
        );
        let idx = router.maybe_escalate_for_context(0, 999_999);
        assert_eq!(
            idx, 0,
            "should not escalate when context_window returns None"
        );
        assert_eq!(
            router.metrics.escalations.load(Ordering::Relaxed),
            0,
            "escalation counter must not increment"
        );
    }

    #[test]
    fn escalate_no_op_within_80_percent() {
        // 800 tokens, 1000-token window → 80% exactly, no escalation.
        let router = TriageRouter::new(
            triage_mock(r#"{"tier":"simple"}"#),
            vec![(ComplexityTier::Simple, ollama_with_window(1000))],
            5,
            50,
        );
        let idx = router.maybe_escalate_for_context(0, 800);
        assert_eq!(idx, 0);
        assert_eq!(router.metrics.escalations.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn escalate_triggers_above_80_percent() {
        // 900 tokens, 1000-token window → above 80%, must escalate to larger provider.
        let router = TriageRouter::new(
            triage_mock(r#"{"tier":"simple"}"#),
            vec![
                (ComplexityTier::Simple, ollama_with_window(1000)),
                (ComplexityTier::Expert, ollama_with_window(10_000)),
            ],
            5,
            50,
        );
        let idx = router.maybe_escalate_for_context(0, 900);
        assert_eq!(idx, 1, "should escalate to the large provider");
        assert_eq!(
            router.metrics.escalations.load(Ordering::Relaxed),
            1,
            "escalation counter must increment"
        );
    }

    #[test]
    fn escalate_no_larger_provider_keeps_original() {
        // Only one provider, context exceeds 80% — cannot escalate, stay put.
        let router = TriageRouter::new(
            triage_mock(r#"{"tier":"simple"}"#),
            vec![(ComplexityTier::Simple, ollama_with_window(100))],
            5,
            50,
        );
        // 99 tokens out of 100 window → above 80% (>= 80 threshold), but no larger provider
        let idx = router.maybe_escalate_for_context(0, 99);
        assert_eq!(idx, 0);
    }

    // ── last_usage delegation ─────────────────────────────────────────────────

    #[test]
    fn last_usage_none_before_any_call() {
        let router = TriageRouter::new(
            triage_mock(r#"{"tier":"simple"}"#),
            vec![(ComplexityTier::Simple, mock_provider("p"))],
            5,
            50,
        );
        assert_eq!(router.last_usage(), None);
        assert_eq!(router.last_cache_usage(), None);
    }

    // ── embed delegation through tier providers ───────────────────────────────

    fn mock_with_embedding(embedding: Vec<f32>) -> AnyProvider {
        let mut p = MockProvider::default();
        p.supports_embeddings = true;
        p.embedding = embedding;
        AnyProvider::Mock(p)
    }

    #[test]
    fn supports_embeddings_false_when_no_tier_supports_it() {
        let router = TriageRouter::new(
            triage_mock(r#"{"tier":"simple"}"#),
            vec![
                (ComplexityTier::Simple, mock_provider("a")),
                (ComplexityTier::Expert, mock_provider("b")),
            ],
            5,
            50,
        );
        assert!(!router.supports_embeddings());
    }

    #[test]
    fn supports_embeddings_true_when_tier_supports_it() {
        let router = TriageRouter::new(
            triage_mock(r#"{"tier":"simple"}"#),
            vec![
                (ComplexityTier::Simple, mock_provider("no-embed")),
                (ComplexityTier::Expert, mock_with_embedding(vec![0.1, 0.2])),
            ],
            5,
            50,
        );
        assert!(router.supports_embeddings());
    }

    #[tokio::test]
    async fn embed_delegates_to_first_embedding_capable_tier() {
        let expected = vec![1.0_f32, 2.0, 3.0];
        let router = TriageRouter::new(
            triage_mock(r#"{"tier":"simple"}"#),
            vec![
                (ComplexityTier::Simple, mock_provider("no-embed")),
                (
                    ComplexityTier::Expert,
                    mock_with_embedding(expected.clone()),
                ),
            ],
            5,
            50,
        );
        let result = router.embed("test query").await.unwrap();
        assert_eq!(result, expected);
    }

    #[tokio::test]
    async fn embed_returns_error_when_no_tier_supports_embeddings() {
        let router = TriageRouter::new(
            triage_mock(r#"{"tier":"simple"}"#),
            vec![(ComplexityTier::Simple, mock_provider("no-embed"))],
            5,
            50,
        );
        let err = router.embed("test").await.unwrap_err();
        assert!(err.to_string().contains("embedding not supported"));
    }

    // ── supports_streaming any-tier ───────────────────────────────────────────

    #[test]
    fn supports_streaming_true_if_any_tier_supports_it() {
        let no_streaming = MockProvider::default(); // streaming: false
        let mut streaming = MockProvider::default();
        streaming.streaming = true;
        let router = TriageRouter::new(
            triage_mock(r#"{"tier":"simple"}"#),
            vec![
                (ComplexityTier::Simple, AnyProvider::Mock(no_streaming)),
                (ComplexityTier::Expert, AnyProvider::Mock(streaming)),
            ],
            5,
            50,
        );
        assert!(router.supports_streaming());
    }

    #[test]
    fn supports_streaming_false_if_no_tier_supports_it() {
        let router = TriageRouter::new(
            triage_mock(r#"{"tier":"simple"}"#),
            vec![
                (
                    ComplexityTier::Simple,
                    AnyProvider::Mock(MockProvider::default()),
                ),
                (
                    ComplexityTier::Expert,
                    AnyProvider::Mock(MockProvider::default()),
                ),
            ],
            5,
            50,
        );
        assert!(!router.supports_streaming());
    }

    // ── debug_request_json reflects last-selected provider (#2229) ────────────

    fn ollama_with_model(model: &str) -> AnyProvider {
        AnyProvider::Ollama(crate::ollama::OllamaProvider::new(
            "http://localhost:11434",
            model.to_owned(),
            String::new(),
        ))
    }

    #[tokio::test]
    async fn debug_request_json_reflects_last_provider_after_chat() {
        // Triage classifies as "expert" → router selects index 1 (expert-model).
        let router = TriageRouter::new(
            triage_mock(r#"{"tier":"expert","reason":"architecture"}"#),
            vec![
                (ComplexityTier::Simple, ollama_with_model("simple-model")),
                (
                    ComplexityTier::Expert,
                    AnyProvider::Mock(MockProvider::with_responses(vec![
                        "expert answer".to_owned(),
                    ])),
                ),
            ],
            5,
            50,
        );
        // Before any chat: falls back to first provider (simple-model via ollama).
        let json_before = router.debug_request_json(&[], &[], false);
        assert_eq!(json_before["model"].as_str().unwrap_or(""), "simple-model");

        // After chat routed to expert tier (index 1): should reflect mock provider (model: null).
        let messages = vec![make_user_msg("design a distributed system")];
        router.chat(&messages).await.unwrap();

        let json_after = router.debug_request_json(&messages, &[], false);
        // Expert tier is MockProvider → debug_request_json returns default (model: null).
        // Simple tier is OllamaProvider → would return model: "simple-model".
        // If the fix is correct, json_after must NOT contain "simple-model".
        assert_ne!(json_after["model"].as_str().unwrap_or(""), "simple-model");
    }

    // ── build_triage_prompt has no context size metadata (#2228) ─────────────

    #[test]
    fn build_triage_prompt_has_no_context_size_metadata() {
        let messages = vec![
            make_user_msg("first message"),
            Message {
                role: Role::Assistant,
                content: "reply".to_owned(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            make_user_msg("second message"),
            make_user_msg("third message"),
        ];
        let prompt = build_triage_prompt(&messages);
        assert!(
            !prompt.contains("messages"),
            "prompt must not contain 'messages' context metadata"
        );
        assert!(
            !prompt.contains("tokens"),
            "prompt must not contain 'tokens' context metadata"
        );
    }
}
