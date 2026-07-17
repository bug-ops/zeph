// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Type-erased provider enum wrapping all concrete backends.
//!
//! [`AnyProvider`] lets callers hold and clone any backend without generics or
//! heap allocation. The macro `delegate_provider!` generates the
//! match-over-variants boilerplate for every [`LlmProvider`] method delegation.
//!
//! # Dynamic dispatch
//!
//! `LlmProvider` is not object-safe (RPIT returns + generic `chat_typed<T>`), so
//! `Box<dyn LlmProvider>` / `Arc<dyn LlmProvider + Send + Sync>` do not compile.
//! The object-safe shadow [`crate::provider_dyn::LlmProviderDyn`] is the current
//! solution; use `Arc<dyn LlmProviderDyn>` wherever dynamic dispatch is required.
//!
//! # TODO (D1 — deferred: migrate call sites from `AnyProvider` to `Arc<dyn LlmProviderDyn>`)
//!
//! `LlmProviderDyn` already exists and works. The remaining work is call-site migration
//! (~880 sites) across feature areas (epic/m49+/anyprovider-deprecation).
//! Each area must be a separate PR; do NOT bundle.

#[cfg(feature = "candle")]
use crate::candle_provider::CandleProvider;
use crate::claude::ClaudeProvider;
#[cfg(feature = "cocoon")]
use crate::cocoon::CocoonProvider;
use crate::compatible::CompatibleProvider;
use crate::gemini::GeminiProvider;
#[cfg(feature = "gonka")]
use crate::gonka::GonkaProvider;
use crate::masking::{MaskedProvider, OutboundMasker};
#[cfg(any(test, feature = "testing"))]
use crate::mock::MockProvider;
use crate::ollama::OllamaProvider;
use crate::openai::OpenAiProvider;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use std::sync::Arc;

use crate::provider::{
    ChatExtras, ChatResponse, ChatStream, GenerationOverrides, LlmProvider, Message, StatusTx,
    ToolDefinition,
};
use crate::router::RouterProvider;
use crate::router::triage::TriageRouter;
use zeph_config::ThinkingConfig;

/// Generates a match over all `AnyProvider` variants, binding the inner provider
/// and evaluating the given closure for each arm.
macro_rules! delegate_provider {
    ($self:expr, |$p:ident| $expr:expr) => {
        match $self {
            AnyProvider::Ollama($p) => $expr,
            AnyProvider::Claude($p) => $expr,
            AnyProvider::OpenAi($p) => $expr,
            AnyProvider::Gemini($p) => $expr,
            #[cfg(feature = "candle")]
            AnyProvider::Candle($p) => $expr,
            AnyProvider::Compatible($p) => $expr,
            AnyProvider::Router($p) => $expr,
            AnyProvider::Triage($p) => $expr,
            #[cfg(feature = "gonka")]
            AnyProvider::Gonka($p) => $expr,
            #[cfg(feature = "cocoon")]
            AnyProvider::Cocoon($p) => $expr,
            #[cfg(any(test, feature = "testing"))]
            AnyProvider::Mock($p) => $expr,
            // #5437: masking is structural — every `LlmProvider` trait method (chat, chat_with_tools,
            // chat_stream, chat_with_extras, debug_request_json, embed, name, ...) routes through
            // this single macro arm, so `MaskedProvider`'s own `LlmProvider` impl (which masks
            // outbound messages before delegating to its inner provider) covers all of them for
            // free — no per-method or per-call-site enumeration needed.
            AnyProvider::Masked($p) => $expr,
        }
    };
}

/// Type-erased enum over all supported LLM backends.
///
/// All variants implement [`LlmProvider`] — `AnyProvider` delegates every trait method
/// to its inner variant via the `delegate_provider!` macro. This avoids heap allocation
/// while retaining the ability to hold multiple provider types in a `Vec` or struct field.
/// For dynamic dispatch across an unknown set of backends, prefer
/// [`Arc<dyn LlmProviderDyn>`](crate::provider_dyn::LlmProviderDyn).
///
/// The `Candle` variant is only available when the `candle` feature is enabled.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum AnyProvider {
    Ollama(OllamaProvider),
    Claude(ClaudeProvider),
    OpenAi(OpenAiProvider),
    Gemini(GeminiProvider),
    #[cfg(feature = "candle")]
    Candle(CandleProvider),
    Compatible(CompatibleProvider),
    Router(Box<RouterProvider>),
    /// Complexity triage router: pre-classifies each request and delegates to the appropriate tier.
    Triage(Box<TriageRouter>),
    /// Gonka native inference provider — routes signed requests through the Gonka network.
    ///
    /// Only available when the `gonka` feature is enabled.
    #[cfg(feature = "gonka")]
    Gonka(GonkaProvider),
    /// Cocoon confidential compute provider — routes requests through the TEE sidecar.
    ///
    /// Only available when the `cocoon` feature is enabled.
    #[cfg(feature = "cocoon")]
    Cocoon(CocoonProvider),
    /// A mock provider for use in tests and benchmarks.
    ///
    /// Only available with the `testing` feature or in `#[cfg(test)]` contexts.
    #[cfg(any(test, feature = "testing"))]
    Mock(MockProvider),
    /// Wraps another provider so every outbound `chat*` call masks registered secrets from
    /// message text before the request leaves the process (#5437).
    ///
    /// Constructed via [`AnyProvider::masked`], injected once at the point an `AnyProvider` is
    /// built (`zeph_core::provider_factory::build_provider_from_entry`) — this is a structural
    /// choke point, not a per-call-site opt-in.
    Masked(Box<MaskedProvider>),
}

/// Runtime reasoning-effort level for the `/reasoning-effort` slash command.
///
/// Owned by `zeph-llm` (the crate that owns [`AnyProvider`]) rather than `zeph-commands`, so
/// lower layers own the domain type and the command layer only parses into it. Mapped
/// per-backend by [`AnyProvider::apply_reasoning_effort`]: Claude → `ThinkingConfig::Adaptive`,
/// OpenAI/Compatible → the existing string-based `reasoning_effort` field, Gemini →
/// `thinking_level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffort {
    /// Minimal reasoning depth; fastest responses.
    Low,
    /// Balanced reasoning depth.
    Medium,
    /// Maximum reasoning depth; slowest responses.
    High,
}

impl ReasoningEffort {
    /// Return the lowercase string representation (`"low"`, `"medium"`, `"high"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl From<ReasoningEffort> for zeph_config::ThinkingEffort {
    fn from(effort: ReasoningEffort) -> Self {
        match effort {
            ReasoningEffort::Low => Self::Low,
            ReasoningEffort::Medium => Self::Medium,
            ReasoningEffort::High => Self::High,
        }
    }
}

impl From<ReasoningEffort> for zeph_config::GeminiThinkingLevel {
    fn from(effort: ReasoningEffort) -> Self {
        match effort {
            ReasoningEffort::Low => Self::Low,
            ReasoningEffort::Medium => Self::Medium,
            ReasoningEffort::High => Self::High,
        }
    }
}

impl std::str::FromStr for ReasoningEffort {
    type Err = String;

    /// Parse a reasoning-effort level from a case-insensitive string.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_llm::any::ReasoningEffort;
    ///
    /// assert_eq!("HIGH".parse::<ReasoningEffort>(), Ok(ReasoningEffort::High));
    /// assert!("minimal".parse::<ReasoningEffort>().is_err());
    /// ```
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            other => Err(format!(
                "unknown reasoning effort '{other}' — expected low|medium|high"
            )),
        }
    }
}

impl AnyProvider {
    /// Wrap `self` so every outbound `chat*`/`chat_with_tools*` call masks message text via
    /// `masker` before the request reaches the inner provider.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use std::sync::Arc;
    /// use zeph_llm::any::AnyProvider;
    /// use zeph_llm::masking::OutboundMasker;
    /// use zeph_llm::ollama::OllamaProvider;
    ///
    /// #[derive(Debug)]
    /// struct NoopMasker;
    /// impl OutboundMasker for NoopMasker {
    ///     fn mask(&self, _text: &str) -> Option<String> { None }
    /// }
    ///
    /// let provider = AnyProvider::Ollama(OllamaProvider::new("http://localhost:11434", "m".into(), "e".into()));
    /// let masked = provider.masked(Arc::new(NoopMasker));
    /// assert!(matches!(masked, AnyProvider::Masked(_)));
    /// ```
    #[must_use]
    pub fn masked(self, masker: Arc<dyn OutboundMasker>) -> Self {
        Self::Masked(Box::new(MaskedProvider::new(self, masker)))
    }

    /// Number of outbound calls that had at least one secret masked, or `None` when this
    /// provider is not wrapped via [`AnyProvider::masked`]. Exposed for the
    /// `secret_mask_applied` observability metric.
    #[must_use]
    pub fn masked_call_count(&self) -> Option<u64> {
        match self {
            Self::Masked(p) => Some(p.applied_count()),
            _ => None,
        }
    }

    /// Set the MAR memory recall confidence for the current turn.
    ///
    /// Delegates to [`RouterProvider::set_memory_confidence`] when the inner provider is
    /// a bandit router. No-op for all other provider types.
    ///
    /// Prefer importing [`RouterAware`][crate::router::RouterAware] for explicit dispatch
    /// at call sites that always work with a known router provider.
    pub fn set_memory_confidence(&self, confidence: Option<f32>) {
        match self {
            AnyProvider::Router(r) => r.set_memory_confidence(confidence),
            AnyProvider::Masked(p) => p.inner().set_memory_confidence(confidence),
            _ => {
                tracing::trace!(
                    provider_variant = self.name(),
                    confidence = ?confidence,
                    "set_memory_confidence: no-op (non-router provider; MAR signal requires RouterProvider)"
                );
            }
        }
    }

    /// Return a cloneable closure that calls `embed()` on this provider.
    pub fn embed_fn(&self) -> impl Fn(&str) -> crate::provider::EmbedFuture + Send + Sync + use<> {
        let provider = std::sync::Arc::new(self.clone());
        move |text: &str| -> crate::provider::EmbedFuture {
            let p = std::sync::Arc::clone(&provider);
            let owned = text.to_owned();
            Box::pin(async move { p.embed(&owned).await })
        }
    }

    /// # Errors
    ///
    /// Returns an error if the provider fails or the response cannot be parsed.
    #[tracing::instrument(name = "llm.any.chat_typed_erased", skip_all)]
    pub async fn chat_typed_erased<T>(&self, messages: &[Message]) -> Result<T, crate::LlmError>
    where
        T: DeserializeOwned + JsonSchema + 'static,
    {
        delegate_provider!(self, |p| p.chat_typed::<T>(messages).await)
    }

    /// Fetch available models from this provider and update the disk cache.
    ///
    /// Returns an empty list for providers that do not support remote model discovery
    /// (Candle) without returning an error.
    ///
    /// # Errors
    ///
    /// Returns an error if the remote request fails.
    #[tracing::instrument(name = "llm.any.list_models_remote", skip_all)]
    pub async fn list_models_remote(
        &self,
    ) -> Result<Vec<crate::model_cache::RemoteModelInfo>, crate::LlmError> {
        match self {
            AnyProvider::Ollama(p) => p.list_models_remote().await,
            AnyProvider::Claude(p) => p.list_models_remote().await,
            AnyProvider::OpenAi(p) => p.list_models_remote().await,
            AnyProvider::Compatible(p) => p.list_models_remote().await,
            AnyProvider::Gemini(p) => p.list_models_remote().await,
            // Router uses synchronous list_models() to avoid recursive async cycles.
            // Results reflect config-time model lists (potentially stale vs. live remote data).
            AnyProvider::Router(p) => {
                tracing::debug!(
                    "list_models_remote: Router falling back to sync list_models (config-time data)"
                );
                Ok(p.list_models()
                    .into_iter()
                    .map(|id| crate::model_cache::RemoteModelInfo {
                        display_name: id.clone(),
                        id,
                        context_window: None,
                        created_at: None,
                    })
                    .collect())
            }
            // Triage delegates list_models to the first tier provider (best effort).
            AnyProvider::Triage(p) => Ok(p
                .name()
                .split(':')
                .next()
                .map(|_| {
                    vec![crate::model_cache::RemoteModelInfo {
                        display_name: p.name().to_owned(),
                        id: p.name().to_owned(),
                        context_window: p.context_window(),
                        created_at: None,
                    }]
                })
                .unwrap_or_default()),
            #[cfg(feature = "candle")]
            AnyProvider::Candle(_) => Ok(vec![]),
            // Gonka nodes have no model discovery API.
            #[cfg(feature = "gonka")]
            AnyProvider::Gonka(_) => Ok(vec![]),
            // Cocoon model discovery is done via CocoonClient::list_models(), not LlmProvider.
            #[cfg(feature = "cocoon")]
            AnyProvider::Cocoon(_) => Ok(vec![]),
            #[cfg(any(test, feature = "testing"))]
            AnyProvider::Mock(p) => Ok(p.models.clone()),
            // Model discovery is not message content — recurse into the inner provider
            // unmasked, no secrets involved.
            AnyProvider::Masked(p) => Box::pin(p.inner().list_models_remote()).await,
        }
    }

    /// Persist router state to disk if this provider is a `RouterProvider`.
    ///
    /// Saves Thompson, reputation, and bandit state concurrently using
    /// [`tokio::task::spawn_blocking`]. No-op for all other provider variants.
    #[tracing::instrument(name = "llm.any.save_router_state", skip_all)]
    pub async fn save_router_state(&self) {
        match self {
            Self::Router(p) => {
                // Run all three saves concurrently — each is independent I/O.
                tokio::join!(
                    p.save_thompson_state(),
                    p.save_reputation_state(),
                    p.save_bandit_state(),
                );
            }
            Self::Masked(p) => Box::pin(p.inner().save_router_state()).await,
            _ => {}
        }
    }

    /// Returns a static string identifying the provider kind for cost/logging purposes.
    ///
    /// Returns `"ollama"` or `"candle"` for local inference providers (no API cost),
    /// `"local"` for providers that are always unpriced (Compatible, Triage),
    /// and `"cloud"` for metered API providers (`Claude`, `OpenAI`, `Gemini`).
    ///
    /// For `Router`, delegates to the last-selected child provider so that cost tracking
    /// correctly attributes API costs even when Thompson/Cascade routing is active.
    #[must_use]
    pub fn provider_kind_str(&self) -> &'static str {
        match self {
            Self::Ollama(_) => "ollama",
            #[cfg(feature = "candle")]
            Self::Candle(_) => "candle",
            // Compatible targets LM Studio / vLLM / llama.cpp — always local, never metered.
            Self::Compatible(_) => "local",
            // Router: delegate to the last-selected child so cost flows to the real provider.
            Self::Router(r) => r.last_selected_provider_kind(),
            // Triage has no post-call provider tracking; treat as unpriced.
            Self::Triage(_) => "local",
            // Gonka is a metered network — treat as cloud for cost tracking.
            #[cfg(feature = "gonka")]
            Self::Gonka(_) => "cloud",
            // Cocoon is a metered TEE network — treat as cloud for cost tracking.
            #[cfg(feature = "cocoon")]
            Self::Cocoon(_) => "cloud",
            Self::Masked(p) => p.inner().provider_kind_str(),
            _ => "cloud",
        }
    }

    /// Send a streaming tool-use request, returning a [`crate::sse::ToolSseStream`].
    ///
    /// Only `Claude` variants support native SSE tool-use streaming — all other providers
    /// return `Err(LlmError::Unavailable)` and callers should fall back to `chat_with_tools`.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider does not support tool streaming or the HTTP request fails.
    #[tracing::instrument(name = "llm.any.chat_with_tools_stream", skip_all)]
    pub async fn chat_with_tools_stream(
        &self,
        messages: &[crate::provider::Message],
        tools: &[crate::provider::ToolDefinition],
    ) -> Result<crate::sse::ToolSseStream, crate::LlmError> {
        match self {
            AnyProvider::Claude(p) => p.chat_with_tools_stream(messages, tools).await,
            AnyProvider::Masked(p) => {
                let masked = p.mask_messages(messages);
                Box::pin(
                    p.inner()
                        .chat_with_tools_stream(masked.as_deref().unwrap_or(messages), tools),
                )
                .await
            }
            _ => Err(crate::LlmError::Unavailable),
        }
    }

    /// Record a quality outcome for reputation-based routing (RAPS).
    ///
    /// Delegates to [`RouterProvider::record_quality_outcome`] when the inner provider is a
    /// router. No-op for all other provider types — this is intentional: quality signals only
    /// apply to multi-provider routers with reputation tracking enabled.
    ///
    /// Must only be called for semantic failures (bad tool arguments, parse errors),
    /// never for network errors or transient failures.
    pub fn record_quality_outcome(&self, provider_name: &str, success: bool) {
        match self {
            Self::Router(p) => p.record_quality_outcome(provider_name, success),
            Self::Masked(p) => p.inner().record_quality_outcome(provider_name, success),
            _ => {
                tracing::trace!(
                    provider_name,
                    success,
                    provider_variant = self.name(),
                    "record_quality_outcome: no-op (non-router provider; quality signals require RouterProvider)"
                );
            }
        }
    }

    /// Return Thompson Sampling distribution snapshots `(provider, alpha, beta)`.
    ///
    /// Returns an empty vec for non-router providers or EMA strategy.
    #[must_use]
    pub fn router_thompson_stats(&self) -> Vec<(String, f64, f64)> {
        match self {
            Self::Router(p) => p.thompson_stats(),
            Self::Masked(p) => p.inner().router_thompson_stats(),
            _ => vec![],
        }
    }

    /// Clone and patch this provider with generation parameter overrides.
    ///
    /// Used by the experiment engine to evaluate each variation with its specific parameters.
    /// `Router` and `Triage` variants are returned unchanged (overrides not supported).
    #[must_use]
    pub fn with_generation_overrides(self, overrides: GenerationOverrides) -> Self {
        match self {
            Self::Ollama(p) => Self::Ollama(p.with_generation_overrides(overrides)),
            Self::Claude(p) => Self::Claude(p.with_generation_overrides(overrides)),
            Self::OpenAi(p) => Self::OpenAi(p.with_generation_overrides(overrides)),
            Self::Gemini(p) => Self::Gemini(p.with_generation_overrides(overrides)),
            Self::Compatible(p) => Self::Compatible(p.with_generation_overrides(overrides)),
            #[cfg(any(test, feature = "testing"))]
            Self::Mock(p) => Self::Mock(p.with_generation_overrides(overrides)),
            #[cfg(feature = "candle")]
            Self::Candle(p) => {
                tracing::warn!("generation overrides not supported for Candle provider");
                Self::Candle(p)
            }
            #[cfg(feature = "gonka")]
            Self::Gonka(p) => Self::Gonka(p.with_generation_overrides(overrides)),
            #[cfg(feature = "cocoon")]
            Self::Cocoon(p) => Self::Cocoon(p.with_generation_overrides(overrides)),
            Self::Router(_) | Self::Triage(_) => {
                tracing::warn!("generation overrides not supported for this provider variant");
                self
            }
            Self::Masked(p) => {
                let inner = p.inner().clone();
                let masker = Arc::clone(&p.masker);
                Self::Masked(Box::new(MaskedProvider::new(
                    inner.with_generation_overrides(overrides),
                    masker,
                )))
            }
        }
    }

    /// Return a clone of this provider with prompt-cache emission suppressed.
    ///
    /// For Claude, this disables explicit `cache_control` emission in outgoing requests —
    /// the Checker's requests will not carry cache-control markers, preventing cache sharing
    /// with the Solver's requests (MARCH asymmetry invariant).
    ///
    /// For `OpenAI`: uses automatic server-side prompt caching triggered by request shape;
    /// there is no `cache_control` field to suppress in the request body. This method is a
    /// documented no-op clone for `OpenAI` — cache separation relies on the distinct
    /// system prompts used by Proposer and Checker, which produce different cache keys.
    ///
    /// For Ollama, Candle, Gemini, and all other providers: no-op clone.
    #[must_use]
    pub fn with_prompt_cache_disabled(&self) -> Self {
        match self {
            Self::Claude(p) => Self::Claude(p.clone().with_cache_user_messages(false)),
            Self::Masked(p) => Self::Masked(Box::new(MaskedProvider::new(
                p.inner().with_prompt_cache_disabled(),
                Arc::clone(&p.masker),
            ))),
            // OpenAI: no request-body opt-out for server-side automatic caching; no-op clone.
            // Cache separation is achieved via distinct system prompts (Proposer ≠ Checker).
            other => other.clone(),
        }
    }

    /// Apply a `reasoning_effort` override to the active provider.
    ///
    /// Delegates to [`OpenAiProvider::set_reasoning_effort`] when the inner variant is
    /// `AnyProvider::OpenAi` or `AnyProvider::Compatible` (which wraps an [`OpenAiProvider`]
    /// internally). No-op for all other provider types — `reasoning_effort` is an OpenAI-specific
    /// parameter and has no equivalent in Ollama, Claude, Gemini, etc.
    ///
    /// Called by the session restore path after a provider switch to propagate a persisted
    /// effort level (e.g. `"high"`) into the live provider instance.
    pub fn set_reasoning_effort(&mut self, effort: Option<String>) {
        match self {
            Self::OpenAi(p) => p.set_reasoning_effort(effort),
            Self::Compatible(p) => p.set_reasoning_effort(effort),
            Self::Masked(p) => p.inner.set_reasoning_effort(effort),
            _ => {}
        }
    }

    /// Set the runtime thinking-token budget on the active provider (`/think-tokens`).
    ///
    /// `None` disables thinking, mapped to each backend's native "off" representation:
    /// Claude clears its `thinking` config and restores `max_tokens` to the construction-time
    /// baseline (see [`crate::claude::ClaudeProvider::set_thinking`]); Gemini maps `None` to
    /// `Some(0)`, its explicit disable value — never to `None`/unset, which could silently
    /// re-enable thinking at the config default. `Some(n)` sets an explicit token budget.
    ///
    /// Only `Claude` and `Gemini` support a thinking-token budget. All other provider
    /// variants return [`crate::LlmError::ModelCapabilityMismatch`] — this command never silently
    /// no-ops.
    ///
    /// `Self::Router` and `Self::Triage` delegate to their applicable inner provider (the one
    /// that served the most recent call, or a deterministic fallback if none has yet) via
    /// their internal `set_thinking_budget_delegated` helpers. Use
    /// [`Self::capability_delegation_advisory`] after a successful call to check whether the
    /// routing strategy may pick a different inner provider on the next turn.
    ///
    /// # Errors
    ///
    /// Returns an error when `budget` is outside the active provider's valid range, or when
    /// the active provider does not support a thinking-token budget.
    pub fn set_thinking_budget(&mut self, budget: Option<u32>) -> Result<(), crate::LlmError> {
        match self {
            Self::Claude(p) => {
                let thinking = budget.map(|n| ThinkingConfig::Extended { budget_tokens: n });
                p.set_thinking(thinking)
            }
            Self::Gemini(p) => {
                let i = match budget {
                    // M1: Gemini's explicit disable value, never `None` (which means
                    // "unset — fall back to config default" and could silently re-enable
                    // thinking, the opposite of what the user asked for).
                    None => 0,
                    Some(n) => i32::try_from(n).unwrap_or(i32::MAX),
                };
                p.set_thinking_budget(Some(i))
            }
            Self::Masked(p) => p.inner.set_thinking_budget(budget),
            Self::Router(r) => r.set_thinking_budget_delegated(budget),
            Self::Triage(t) => t.set_thinking_budget_delegated(budget),
            other => Err(crate::LlmError::ModelCapabilityMismatch {
                provider: other.name().to_owned(),
                message: "does not support a thinking-token budget".into(),
            }),
        }
    }

    /// Apply a `/reasoning-effort` level to the active provider, mapped per-backend:
    ///
    /// - `Claude` → `ThinkingConfig::Adaptive { effort }`, overriding any active `Extended`
    ///   token budget (the two are mutually-exclusive variants of the same `thinking` field —
    ///   see [`Self::set_thinking_budget`]).
    /// - `OpenAI` / `Compatible` → the existing string-based `reasoning_effort` field.
    /// - `Gemini` → `thinking_level`.
    /// - `Router` / `Triage` → delegates to the applicable inner provider, mirroring
    ///   [`Self::set_thinking_budget`]'s delegation behavior.
    /// - All other providers → [`crate::LlmError::ModelCapabilityMismatch`].
    ///
    /// This is the new session-only runtime entry point for `/reasoning-effort`. It is
    /// deliberately separate from [`Self::set_reasoning_effort`] (the OpenAI-only,
    /// string-based persistence-restore path) — merging them would blur this feature's
    /// session-only scope boundary.
    ///
    /// # Errors
    ///
    /// Returns an error when the active provider does not support a reasoning-effort level.
    pub fn apply_reasoning_effort(
        &mut self,
        effort: ReasoningEffort,
    ) -> Result<(), crate::LlmError> {
        match self {
            Self::Claude(p) => p.set_thinking(Some(ThinkingConfig::Adaptive {
                effort: Some(effort.into()),
            })),
            Self::OpenAi(p) => {
                p.set_reasoning_effort(Some(effort.as_str().to_owned()));
                Ok(())
            }
            Self::Compatible(p) => {
                p.set_reasoning_effort(Some(effort.as_str().to_owned()));
                Ok(())
            }
            Self::Gemini(p) => {
                p.set_thinking_level(Some(effort.into()));
                Ok(())
            }
            Self::Masked(p) => p.inner.apply_reasoning_effort(effort),
            Self::Router(r) => r.apply_reasoning_effort_delegated(effort),
            Self::Triage(t) => t.apply_reasoning_effort_delegated(effort),
            other => Err(crate::LlmError::ModelCapabilityMismatch {
                provider: other.name().to_owned(),
                message: "does not support reasoning effort".into(),
            }),
        }
    }

    /// Return the current thinking-token budget on the active provider, if any is set.
    ///
    /// `None` means thinking is disabled, the provider is in effort-based mode (Claude
    /// `Adaptive`), or the provider does not support a token budget at all. Used by the
    /// `/think-tokens` no-arg display path and by the `/provider` switch's reset-notice check.
    ///
    /// `Self::Router` and `Self::Triage` return the value from their applicable inner
    /// provider (see [`Self::set_thinking_budget`]).
    #[must_use]
    pub fn current_thinking_budget(&self) -> Option<u32> {
        match self {
            Self::Claude(p) => p.current_thinking_budget(),
            // `0` (disabled) and `-1` (dynamic, unreachable via /think-tokens per M1) both
            // display as "off" — only a positive explicit budget is shown as a number.
            Self::Gemini(p) => p
                .current_thinking_budget()
                .and_then(|b| u32::try_from(b).ok())
                .filter(|&b| b > 0),
            Self::Masked(p) => p.inner().current_thinking_budget(),
            Self::Router(r) => r.current_thinking_budget_delegated(),
            Self::Triage(t) => t.current_thinking_budget_delegated(),
            _ => None,
        }
    }

    /// Return the current reasoning-effort level on the active provider, if any is set.
    ///
    /// `None` means no effort level is set, the provider is in token-budget mode (Claude
    /// `Extended`), or the provider does not support an effort level at all. Used by the
    /// `/reasoning-effort` no-arg display path and by the `/provider` switch's reset-notice
    /// check.
    ///
    /// `Self::Router` and `Self::Triage` return the value from their applicable inner
    /// provider (see [`Self::set_thinking_budget`]).
    #[must_use]
    pub fn current_reasoning_effort(&self) -> Option<String> {
        match self {
            Self::Claude(p) => p.current_reasoning_effort(),
            Self::OpenAi(p) => p.reasoning_effort.clone(),
            Self::Compatible(p) => p.current_reasoning_effort(),
            Self::Gemini(p) => p.current_reasoning_effort(),
            Self::Masked(p) => p.inner().current_reasoning_effort(),
            Self::Router(r) => r.current_reasoning_effort_delegated(),
            Self::Triage(t) => t.current_reasoning_effort_delegated(),
            _ => None,
        }
    }

    /// Returns a short advisory when the active provider is a routed pool (`Self::Router` /
    /// `Self::Triage`) whose selection strategy may pick a different inner provider on the
    /// next turn than the one a mutating capability command ([`Self::set_thinking_budget`],
    /// [`Self::apply_reasoning_effort`]) just configured.
    ///
    /// `None` for non-routed variants, deterministic strategies (`RouterStrategy::Cascade`),
    /// and degenerate single-provider pools — in those cases the applicable provider on the
    /// next dispatch is guaranteed to be the same one the command just mutated, so no warning
    /// is needed. See spec `071-router-thinking-budget-delegation` §5 for the resolved
    /// edge-case behavior this implements.
    #[must_use]
    pub fn capability_delegation_advisory(&self) -> Option<String> {
        match self {
            Self::Router(r) => r.capability_delegation_advisory(),
            Self::Triage(t) => t.capability_delegation_advisory(),
            Self::Masked(p) => p.inner().capability_delegation_advisory(),
            _ => None,
        }
    }

    /// Propagate a status sender to the inner provider (where supported).
    pub fn set_status_tx(&mut self, tx: StatusTx) {
        match self {
            Self::Claude(p) => {
                p.status_tx = Some(tx);
            }
            Self::OpenAi(p) => {
                p.status_tx = Some(tx);
            }
            Self::Compatible(p) => {
                p.set_status_tx(tx);
            }
            Self::Router(p) => {
                p.set_status_tx(tx);
            }
            Self::Gemini(p) => {
                p.set_status_tx(tx);
            }
            Self::Triage(p) => {
                p.set_status_tx(&tx);
            }
            Self::Ollama(_) => {}
            #[cfg(feature = "candle")]
            Self::Candle(_) => {}
            #[cfg(feature = "gonka")]
            Self::Gonka(p) => {
                p.set_status_tx(tx);
            }
            #[cfg(feature = "cocoon")]
            Self::Cocoon(p) => {
                p.set_status_tx(tx);
            }
            #[cfg(any(test, feature = "testing"))]
            Self::Mock(_) => {}
            Self::Masked(p) => {
                p.inner.set_status_tx(tx);
            }
        }
    }
}

impl LlmProvider for AnyProvider {
    fn context_window(&self) -> Option<usize> {
        delegate_provider!(self, |p| p.context_window())
    }

    async fn chat(&self, messages: &[Message]) -> Result<String, crate::LlmError> {
        delegate_provider!(self, |p| p.chat(messages).await)
    }

    async fn chat_with_extras(
        &self,
        messages: &[Message],
    ) -> Result<(String, ChatExtras), crate::LlmError> {
        delegate_provider!(self, |p| p.chat_with_extras(messages).await)
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, crate::LlmError> {
        delegate_provider!(self, |p| p.chat_stream(messages).await)
    }

    fn supports_streaming(&self) -> bool {
        delegate_provider!(self, |p| p.supports_streaming())
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::LlmError> {
        delegate_provider!(self, |p| p.embed(text).await)
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, crate::LlmError> {
        delegate_provider!(self, |p| p.embed_batch(texts).await)
    }

    fn supports_embeddings(&self) -> bool {
        delegate_provider!(self, |p| p.supports_embeddings())
    }

    fn name(&self) -> &str {
        delegate_provider!(self, |p| p.name())
    }

    fn model_identifier(&self) -> &str {
        delegate_provider!(self, |p| p.model_identifier())
    }

    fn effective_model_identifier(&self) -> &str {
        delegate_provider!(self, |p| p.effective_model_identifier())
    }

    fn supports_structured_output(&self) -> bool {
        delegate_provider!(self, |p| p.supports_structured_output())
    }

    fn supports_vision(&self) -> bool {
        delegate_provider!(self, |p| p.supports_vision())
    }

    fn supports_tool_use(&self) -> bool {
        delegate_provider!(self, |p| p.supports_tool_use())
    }

    async fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, crate::LlmError> {
        delegate_provider!(self, |p| p.chat_with_tools(messages, tools).await)
    }

    fn last_cache_usage(&self) -> Option<(u64, u64)> {
        delegate_provider!(self, |p| p.last_cache_usage())
    }

    fn last_usage(&self) -> Option<(u64, u64)> {
        delegate_provider!(self, |p| p.last_usage())
    }

    fn debug_request_json(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream: bool,
    ) -> serde_json::Value {
        delegate_provider!(self, |p| p.debug_request_json(messages, tools, stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::ClaudeProvider;
    use crate::ollama::OllamaProvider;
    use crate::provider::MessageMetadata;
    use crate::provider::Role;

    #[test]
    fn any_ollama_context_window_delegates() {
        let mut ollama =
            OllamaProvider::new("http://localhost:11434", "test".into(), "embed".into());
        ollama.set_context_window(8192);
        let provider = AnyProvider::Ollama(ollama);
        assert_eq!(provider.context_window(), Some(8192));
    }

    #[test]
    fn any_claude_context_window_delegates() {
        let provider = AnyProvider::Claude(ClaudeProvider::new(
            "key".into(),
            "claude-sonnet-4-5".into(),
            1024,
        ));
        assert_eq!(provider.context_window(), Some(200_000));
    }

    #[test]
    fn any_ollama_name() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        assert_eq!(provider.name(), "ollama");
    }

    #[test]
    fn any_claude_name() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        assert_eq!(provider.name(), "claude");
    }

    #[test]
    fn any_ollama_supports_streaming() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        assert!(provider.supports_streaming());
    }

    #[test]
    fn any_claude_supports_streaming() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        assert!(provider.supports_streaming());
    }

    #[test]
    fn any_ollama_supports_embeddings() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        assert!(provider.supports_embeddings());
    }

    #[test]
    fn any_claude_does_not_support_embeddings() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        assert!(!provider.supports_embeddings());
    }

    #[test]
    fn any_ollama_debug() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        let debug = format!("{provider:?}");
        assert!(debug.contains("Ollama"));
    }

    #[test]
    fn any_claude_debug() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        let debug = format!("{provider:?}");
        assert!(debug.contains("Claude"));
    }

    #[test]
    fn any_ollama_clone() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        let cloned = provider.clone();
        assert_eq!(cloned.name(), "ollama");
    }

    #[test]
    fn any_claude_clone() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        let cloned = provider.clone();
        assert_eq!(cloned.name(), "claude");
    }

    #[tokio::test]
    async fn any_claude_embed_returns_error() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        let result = provider.embed("test").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn any_ollama_chat_unreachable_errors() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:1",
            "test".into(),
            "embed".into(),
        ));
        let messages = vec![Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let result = provider.chat(&messages).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn any_claude_chat_unreachable_errors() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        let messages = vec![Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let result = provider.chat(&messages).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn any_ollama_chat_stream_unreachable_errors() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:1",
            "test".into(),
            "embed".into(),
        ));
        let messages = vec![Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let result = provider.chat_stream(&messages).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn any_claude_chat_stream_unreachable_errors() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        let messages = vec![Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let result = provider.chat_stream(&messages).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn any_ollama_embed_unreachable_errors() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:1",
            "test".into(),
            "embed".into(),
        ));
        let result = provider.embed("test").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn any_claude_embed_error_message() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        let result = provider.embed("test").await;
        let err = result.unwrap_err();
        assert!(err.to_string().contains("embedding not supported by"));
    }

    #[test]
    fn any_ollama_name_delegates() {
        let inner = OllamaProvider::new("http://127.0.0.1:1", "m".into(), "e".into());
        let any = AnyProvider::Ollama(inner);
        assert_eq!(any.name(), "ollama");
    }

    #[test]
    fn any_claude_name_delegates() {
        let inner = ClaudeProvider::new("k".into(), "m".into(), 1024);
        let any = AnyProvider::Claude(inner);
        assert_eq!(any.name(), "claude");
    }

    #[test]
    fn any_provider_clone_independence() {
        let original = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 2048));
        let cloned = original.clone();
        assert_eq!(original.name(), cloned.name());
        assert!(original.supports_streaming());
        assert!(cloned.supports_streaming());
    }

    #[test]
    fn any_provider_debug_variants() {
        let ollama = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "m".into(),
            "e".into(),
        ));
        let claude = AnyProvider::Claude(ClaudeProvider::new("k".into(), "m".into(), 1024));
        assert!(format!("{ollama:?}").contains("Ollama"));
        assert!(format!("{claude:?}").contains("Claude"));
    }

    #[test]
    fn any_openai_name() {
        let provider = AnyProvider::OpenAi(crate::openai::OpenAiProvider::new(
            crate::openai::OpenAiConfig {
                api_key: "key".into(),
                base_url: "https://api.openai.com/v1".into(),
                model: "gpt-4o".into(),
                max_tokens: 1024,
                embedding_model: None,
                reasoning_effort: None,
                context_window: None,
                completion_tokens_param: None,
            },
        ));
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn any_openai_supports_streaming() {
        let provider = AnyProvider::OpenAi(crate::openai::OpenAiProvider::new(
            crate::openai::OpenAiConfig {
                api_key: "key".into(),
                base_url: "https://api.openai.com/v1".into(),
                model: "gpt-4o".into(),
                max_tokens: 1024,
                embedding_model: None,
                reasoning_effort: None,
                context_window: None,
                completion_tokens_param: None,
            },
        ));
        assert!(provider.supports_streaming());
    }

    #[test]
    fn any_openai_supports_embeddings() {
        let with_embed = AnyProvider::OpenAi(crate::openai::OpenAiProvider::new(
            crate::openai::OpenAiConfig {
                api_key: "key".into(),
                base_url: "https://api.openai.com/v1".into(),
                model: "gpt-4o".into(),
                max_tokens: 1024,
                embedding_model: Some("text-embedding-3-small".into()),
                reasoning_effort: None,
                context_window: None,
                completion_tokens_param: None,
            },
        ));
        assert!(with_embed.supports_embeddings());

        let without_embed = AnyProvider::OpenAi(crate::openai::OpenAiProvider::new(
            crate::openai::OpenAiConfig {
                api_key: "key".into(),
                base_url: "https://api.openai.com/v1".into(),
                model: "gpt-4o".into(),
                max_tokens: 1024,
                embedding_model: None,
                reasoning_effort: None,
                context_window: None,
                completion_tokens_param: None,
            },
        ));
        assert!(!without_embed.supports_embeddings());
    }

    #[test]
    fn any_openai_debug() {
        let provider = AnyProvider::OpenAi(crate::openai::OpenAiProvider::new(
            crate::openai::OpenAiConfig {
                api_key: "key".into(),
                base_url: "https://api.openai.com/v1".into(),
                model: "gpt-4o".into(),
                max_tokens: 1024,
                embedding_model: None,
                reasoning_effort: None,
                context_window: None,
                completion_tokens_param: None,
            },
        ));
        let debug = format!("{provider:?}");
        assert!(debug.contains("OpenAi"));
    }

    #[tokio::test]
    async fn chat_typed_erased_dispatches_to_mock() {
        #[derive(Debug, serde::Deserialize, schemars::JsonSchema, PartialEq)]
        struct TestOutput {
            value: String,
        }

        let mock =
            crate::mock::MockProvider::with_responses(vec![r#"{"value": "from_mock"}"#.into()]);
        let provider = AnyProvider::Mock(mock);
        let messages = vec![Message::from_legacy(Role::User, "test")];
        let result: TestOutput = provider.chat_typed_erased(&messages).await.unwrap();
        assert_eq!(
            result,
            TestOutput {
                value: "from_mock".into()
            }
        );
    }

    #[test]
    fn any_openai_supports_structured_output() {
        let provider = AnyProvider::OpenAi(crate::openai::OpenAiProvider::new(
            crate::openai::OpenAiConfig {
                api_key: "key".into(),
                base_url: "https://api.openai.com/v1".into(),
                model: "gpt-4o".into(),
                max_tokens: 1024,
                embedding_model: None,
                reasoning_effort: None,
                context_window: None,
                completion_tokens_param: None,
            },
        ));
        assert!(provider.supports_structured_output());
    }

    #[test]
    fn any_ollama_does_not_support_structured_output() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        assert!(!provider.supports_structured_output());
    }

    #[test]
    fn any_claude_supports_vision() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        assert!(provider.supports_vision());
    }

    #[test]
    fn any_openai_supports_vision() {
        let provider = AnyProvider::OpenAi(crate::openai::OpenAiProvider::new(
            crate::openai::OpenAiConfig {
                api_key: "key".into(),
                base_url: "https://api.openai.com/v1".into(),
                model: "gpt-4o".into(),
                max_tokens: 1024,
                embedding_model: None,
                reasoning_effort: None,
                context_window: None,
                completion_tokens_param: None,
            },
        ));
        assert!(provider.supports_vision());
    }

    #[test]
    fn any_ollama_supports_vision_false_by_default() {
        // #6377: a freshly constructed Ollama provider has no confirmed vision capability
        // and no explicit vision_model — AnyProvider delegation must not assume vision support.
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        assert!(!provider.supports_vision());
    }

    #[test]
    fn any_ollama_supports_vision_true_with_vision_model() {
        let provider = AnyProvider::Ollama(
            OllamaProvider::new("http://localhost:11434", "test".into(), "embed".into())
                .with_vision_model("llava:13b".into()),
        );
        assert!(provider.supports_vision());
    }

    #[cfg(feature = "gonka")]
    fn make_gonka() -> AnyProvider {
        use crate::gonka::endpoints::{EndpointPool, GonkaEndpoint};
        use crate::gonka::{GonkaProvider, RequestSigner};
        use std::sync::Arc;
        let signer = Arc::new(
            RequestSigner::from_hex(
                "0000000000000000000000000000000000000000000000000000000000000001",
                "gonka",
            )
            .unwrap(),
        );
        let pool = Arc::new(
            EndpointPool::new(vec![GonkaEndpoint {
                base_url: "https://node1.example.com".into(),
                address: "gonka1w508d6qejxtdg4y5r3zarvary0c5xw7k2gsyg6".into(),
            }])
            .unwrap(),
        );
        AnyProvider::Gonka(GonkaProvider::new(crate::gonka::GonkaConfig {
            signer,
            pool,
            model: "gpt-4o".into(),
            max_tokens: 4096,
            embedding_model: None,
            timeout: std::time::Duration::from_secs(30),
        }))
    }

    #[cfg(feature = "gonka")]
    #[test]
    fn any_gonka_name() {
        assert_eq!(make_gonka().name(), "gonka");
    }

    #[cfg(feature = "gonka")]
    #[test]
    fn any_gonka_supports_streaming() {
        assert!(make_gonka().supports_streaming());
    }

    #[cfg(feature = "gonka")]
    #[test]
    fn any_gonka_provider_kind_str() {
        assert_eq!(make_gonka().provider_kind_str(), "cloud");
    }

    #[cfg(feature = "cocoon")]
    fn make_cocoon() -> AnyProvider {
        use crate::cocoon::{CocoonClient, CocoonProvider};
        use std::sync::Arc;
        let client = Arc::new(CocoonClient::new(
            "http://localhost:10000",
            None,
            std::time::Duration::from_secs(5),
        ));
        AnyProvider::Cocoon(CocoonProvider::new("Qwen/Qwen3-0.6B", 4096, None, client))
    }

    #[cfg(feature = "cocoon")]
    #[test]
    fn any_cocoon_name() {
        assert_eq!(make_cocoon().name(), "cocoon");
    }

    #[cfg(feature = "cocoon")]
    #[test]
    fn any_cocoon_supports_streaming() {
        assert!(make_cocoon().supports_streaming());
    }

    #[cfg(feature = "cocoon")]
    #[test]
    fn any_cocoon_provider_kind_str() {
        assert_eq!(make_cocoon().provider_kind_str(), "cloud");
    }

    #[test]
    fn any_ollama_with_generation_overrides_preserves_variant() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        let overrides = crate::provider::GenerationOverrides {
            temperature: Some(0.3),
            top_p: None,
            top_k: None,
            frequency_penalty: None,
            presence_penalty: None,
        };
        let patched = provider.with_generation_overrides(overrides);
        assert!(
            matches!(patched, AnyProvider::Ollama(_)),
            "variant must remain Ollama after with_generation_overrides"
        );
    }

    // ── ReasoningEffort ──────────────────────────────────────────────────────

    #[test]
    fn reasoning_effort_from_str_case_insensitive() {
        assert_eq!("low".parse(), Ok(ReasoningEffort::Low));
        assert_eq!("MEDIUM".parse(), Ok(ReasoningEffort::Medium));
        assert_eq!("High".parse(), Ok(ReasoningEffort::High));
    }

    #[test]
    fn reasoning_effort_from_str_rejects_unknown() {
        assert!("minimal".parse::<ReasoningEffort>().is_err());
        assert!("".parse::<ReasoningEffort>().is_err());
    }

    #[test]
    fn reasoning_effort_as_str_roundtrip() {
        for effort in [
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
        ] {
            assert_eq!(effort.as_str().parse(), Ok(effort));
        }
    }

    // ── set_thinking_budget / apply_reasoning_effort fan-out ────────────────

    fn openai_provider() -> crate::openai::OpenAiProvider {
        crate::openai::OpenAiProvider::new(crate::openai::OpenAiConfig {
            api_key: "key".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o".into(),
            max_tokens: 1024,
            embedding_model: None,
            reasoning_effort: None,
            context_window: None,
            completion_tokens_param: None,
        })
    }

    fn compatible_provider() -> crate::compatible::CompatibleProvider {
        crate::compatible::CompatibleProvider::new(crate::compatible::CompatibleConfig {
            provider_name: "together".into(),
            api_key: "key".into(),
            base_url: "https://api.together.xyz/v1".into(),
            model: "m".into(),
            max_tokens: 1024,
            embedding_model: None,
            completion_tokens_param: None,
        })
    }

    #[test]
    fn set_thinking_budget_claude_sets_and_clears() {
        let mut provider = AnyProvider::Claude(ClaudeProvider::new("k".into(), "m".into(), 1024));
        provider.set_thinking_budget(Some(8000)).unwrap();
        assert_eq!(provider.current_thinking_budget(), Some(8000));

        provider.set_thinking_budget(None).unwrap();
        assert!(provider.current_thinking_budget().is_none());
    }

    #[test]
    fn set_thinking_budget_gemini_none_maps_to_disable() {
        let mut provider = AnyProvider::Gemini(GeminiProvider::new(
            "k".into(),
            "gemini-2.5-flash".into(),
            1024,
        ));
        provider.set_thinking_budget(Some(1024)).unwrap();
        assert_eq!(provider.current_thinking_budget(), Some(1024));

        // M1: off maps to Gemini's native Some(0) disable value, not None/unset.
        provider.set_thinking_budget(None).unwrap();
        let AnyProvider::Gemini(ref inner) = provider else {
            unreachable!()
        };
        assert_eq!(inner.current_thinking_budget(), Some(0));
        // Display path folds the disabled 0 into "off" (None).
        assert!(provider.current_thinking_budget().is_none());
    }

    #[test]
    fn set_thinking_budget_unsupported_provider_returns_capability_mismatch() {
        let mut provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "m".into(),
            "e".into(),
        ));
        let err = provider.set_thinking_budget(Some(1024)).unwrap_err();
        assert!(matches!(
            err,
            crate::LlmError::ModelCapabilityMismatch { .. }
        ));
        assert!(err.to_string().contains("ollama"), "{err}");
    }

    #[test]
    fn set_thinking_budget_masked_dispatches_to_inner() {
        let inner = AnyProvider::Claude(ClaudeProvider::new("k".into(), "m".into(), 1024));
        let mut provider = inner.masked(std::sync::Arc::new(NoopMasker));
        provider.set_thinking_budget(Some(4000)).unwrap();
        assert_eq!(provider.current_thinking_budget(), Some(4000));
    }

    #[test]
    fn apply_reasoning_effort_claude_sets_adaptive_and_overrides_extended() {
        let mut provider = AnyProvider::Claude(ClaudeProvider::new("k".into(), "m".into(), 1024));
        provider.set_thinking_budget(Some(8000)).unwrap();
        assert_eq!(provider.current_thinking_budget(), Some(8000));

        // Cross-override: Extended and Adaptive share one field on Claude.
        provider
            .apply_reasoning_effort(ReasoningEffort::High)
            .unwrap();
        assert_eq!(provider.current_reasoning_effort().as_deref(), Some("high"));
        assert!(provider.current_thinking_budget().is_none());
    }

    #[test]
    fn apply_reasoning_effort_openai_sets_string_field() {
        let mut provider = AnyProvider::OpenAi(openai_provider());
        provider
            .apply_reasoning_effort(ReasoningEffort::Medium)
            .unwrap();
        assert_eq!(
            provider.current_reasoning_effort().as_deref(),
            Some("medium")
        );
    }

    #[test]
    fn apply_reasoning_effort_compatible_sets_string_field() {
        let mut provider = AnyProvider::Compatible(compatible_provider());
        provider
            .apply_reasoning_effort(ReasoningEffort::Low)
            .unwrap();
        assert_eq!(provider.current_reasoning_effort().as_deref(), Some("low"));
    }

    #[test]
    fn apply_reasoning_effort_gemini_sets_thinking_level() {
        let mut provider =
            AnyProvider::Gemini(GeminiProvider::new("k".into(), "gemini-3-pro".into(), 1024));
        provider
            .apply_reasoning_effort(ReasoningEffort::High)
            .unwrap();
        assert_eq!(provider.current_reasoning_effort().as_deref(), Some("high"));
    }

    #[test]
    fn apply_reasoning_effort_unsupported_provider_returns_capability_mismatch() {
        let mut provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "m".into(),
            "e".into(),
        ));
        let err = provider
            .apply_reasoning_effort(ReasoningEffort::Low)
            .unwrap_err();
        assert!(matches!(
            err,
            crate::LlmError::ModelCapabilityMismatch { .. }
        ));
    }

    #[test]
    fn apply_reasoning_effort_masked_dispatches_to_inner() {
        let inner = AnyProvider::OpenAi(openai_provider());
        let mut provider = inner.masked(std::sync::Arc::new(NoopMasker));
        provider
            .apply_reasoning_effort(ReasoningEffort::High)
            .unwrap();
        assert_eq!(provider.current_reasoning_effort().as_deref(), Some("high"));
    }

    #[test]
    fn current_thinking_budget_and_effort_none_by_default() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "m".into(),
            "e".into(),
        ));
        assert!(provider.current_thinking_budget().is_none());
        assert!(provider.current_reasoning_effort().is_none());
    }

    #[derive(Debug)]
    struct NoopMasker;
    impl crate::masking::OutboundMasker for NoopMasker {
        fn mask(&self, _text: &str) -> Option<String> {
            None
        }
    }
}
