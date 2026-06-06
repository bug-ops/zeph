// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Routing strategy selector and per-strategy configuration structs for
//! [`RouterProvider`](super::RouterProvider).
//!
//! These types form the public configuration surface of the router and are
//! re-exported from [`crate::router`] to preserve their original import paths.

use std::sync::Arc;

use super::cascade::ClassifierMode;

/// Routing strategy used by [`RouterProvider`](super::RouterProvider).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
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
/// See [`bandit`](super::bandit) module for the algorithm details and trade-offs.
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

/// Runtime ASI configuration passed to [`RouterProvider::with_asi`](super::RouterProvider::with_asi).
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
    pub summary_provider: Option<Arc<dyn crate::provider_dyn::LlmProviderDyn>>,
    /// Explicit cost ordering of provider names (cheapest first).
    /// When set, providers are sorted by their position in this list at construction time.
    /// Providers not listed are appended after listed ones in original chain order.
    pub cost_tiers: Option<Vec<String>>,
    /// Hard timeout for the judge LLM call (milliseconds). Default: 5000.
    pub judge_timeout_ms: u64,
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
            judge_timeout_ms: 5_000,
        }
    }
}
