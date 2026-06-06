// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! LLM provider configuration types (moved from `zeph-llm`).
//!
//! This module declares the configuration surface for every LLM backend and the
//! multi-provider routing layer. It is organized into private submodules by provider
//! family, with all public types re-exported here so the historical
//! `zeph_config::providers::*` paths remain stable:
//!
//! - `thinking` — reasoning-effort and prompt-cache enums embedded in provider entries
//! - `llm` — the root `[llm]` config, provider-kind selector, streaming limits, STT
//! - `router` — multi-provider routing strategy configuration
//! - `candle` — local Candle inference backend configuration
//! - `entry` — the unified [`ProviderEntry`] and pool validation

mod candle;
mod entry;
mod llm;
mod router;
mod thinking;

#[cfg(test)]
mod tests;

pub use candle::{
    CandleConfig, CandleDevice, CandleInlineConfig, CandleSource, GenerationParams, MAX_TOKENS_CAP,
};
pub use entry::{CocoonPricing, GonkaNode, ProviderEntry, ProviderOverrides, validate_pool};
pub use llm::{
    FAST_TIER_MODEL_HINTS, LlmConfig, ProviderKind, StreamLimits, SttConfig, default_stt_language,
};
pub use router::{
    AsiConfig, BanditConfig, CascadeClassifierMode, CascadeConfig, CoeConfig,
    ComplexityRoutingConfig, LlmRoutingStrategy, ReputationConfig, RouterConfig,
    RouterStrategyConfig, TierMapping,
};
pub use thinking::{CacheTtl, GeminiThinkingLevel, ThinkingConfig, ThinkingEffort};
pub use zeph_common::ProviderName;

pub(crate) use llm::{
    get_default_embedding_model, get_default_response_cache_ttl_secs, get_default_router_ema_alpha,
    get_default_router_reorder_interval,
};

/// Default value for boolean config fields that default to `true`.
///
/// Shared by the submodules whose serde `default` attributes need a `true` default
/// (e.g. `ProviderEntry::cocoon_health_check`, `ComplexityRoutingConfig::bypass_single_provider`).
fn default_true() -> bool {
    true
}

/// `skip_serializing_if` predicate that omits a boolean field when it is `true`.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_true(v: &bool) -> bool {
    *v
}
