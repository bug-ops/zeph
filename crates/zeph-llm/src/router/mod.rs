// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Multi-provider router with pluggable routing strategies.
//!
//! [`RouterProvider`] implements [`LlmProvider`](crate::provider::LlmProvider) and forwards
//! every call to one of its configured backends, chosen according to the active
//! [`RouterStrategy`].
//!
//! # Routing strategies
//!
//! | Strategy | Module | Description |
//! |---|---|---|
//! | [`RouterStrategy::Ema`] | `crate::ema` | EMA-weighted latency-aware ordering |
//! | [`RouterStrategy::Thompson`] | [`thompson`] | Bayesian Beta-distribution sampling |
//! | [`RouterStrategy::Cascade`] | [`cascade`] | Cheapest-first with quality escalation |
//! | [`RouterStrategy::Bandit`] | [`bandit`] | Contextual `LinUCB` (PILOT algorithm) |
//!
//! Strategies are selected via builder methods on [`RouterProvider`]:
//! - [`RouterProvider::with_ema`]
//! - [`RouterProvider::with_thompson`]
//! - [`RouterProvider::with_cascade`]
//! - [`RouterProvider::with_bandit`]
//!
//! # Reputation-Aware Provider Selection (RAPS)
//!
//! All strategies support an optional Bayesian reputation layer ([`reputation`]) that
//! penalizes providers which produce semantically invalid tool arguments. Enable with
//! [`RouterProvider::with_reputation`].
//!
//! # Agent Stability Index (ASI)
//!
//! An optional session-level coherence tracker ([`asi`]) measures embedding-based
//! response quality and feeds back into Thompson selection. Enable with
//! [`RouterProvider::with_asi`].
//!
//! # Security
//!
//! Thompson and Bandit state files are loaded from user-controlled paths at startup.
//! Files are validated (finite floats, clamped range) and written with `0o600` permissions
//! on Unix. Do not store state files in world-writable directories.

mod builder;
mod chat;
mod config;
mod embed_cache;
mod provider_impl;
mod select;

pub mod asi;
pub mod aware;
pub mod bandit;
pub mod cascade;
pub mod coe;
pub mod reputation;
pub mod state;
pub mod thompson;
pub mod triage;

pub use aware::RouterAware;
pub use config::{AsiRouterConfig, BanditRouterConfig, CascadeRouterConfig, RouterStrategy};
pub use state::RouterState;

pub(crate) use embed_cache::BanditEmbedCache;

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use parking_lot::Mutex;

use asi::AsiState;
use bandit::BanditState;
use cascade::CascadeState;
use coe::CoeRouter;
use reputation::ReputationTracker;
use thompson::ThompsonState;

use crate::ema::EmaTracker;
use crate::provider::StatusTx;

/// Rate-limits the ASI coherence WARN to at most once per 60 seconds process-wide.
static ASI_WARN_LAST_SECS: AtomicU64 = AtomicU64::new(0);

/// Maximum number of concurrent fire-and-forget ASI coherence update tasks.
///
/// When the `JoinSet` reaches this limit, new spawns are skipped (not aborted) to
/// preserve in-flight work. ASI tasks are analytics-only and do not affect
/// memory persistence.
const MAX_ASI_TASKS: usize = 8;

/// Runs `f` without blocking the Tokio executor.
///
/// On a multi-thread runtime uses `block_in_place`; on a `current_thread` runtime (unit
/// tests, single-threaded entry points) falls back to a direct call since there is no
/// executor thread pool to offload to.
fn blocking_load<T>(f: impl FnOnce() -> T) -> T {
    if tokio::runtime::Handle::try_current()
        .is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
    {
        tokio::task::block_in_place(f)
    } else {
        f()
    }
}

/// Returns `true` when any message carries a `MessagePart::Image`.
///
/// Shared by [`triage::TriageRouter`] and [`RouterProvider`]'s vision-tier dispatch safety
/// net (spec-072 C3).
pub(crate) fn messages_contain_image(messages: &[crate::provider::Message]) -> bool {
    messages.iter().any(|m| {
        m.parts
            .iter()
            .any(|p| matches!(p, crate::provider::MessagePart::Image(_)))
    })
}

/// Drop every `MessagePart::Image` from a message set — the text placeholder from the
/// companion `MessagePart::ToolResult` (spec-072 §8 control 2) always remains as the
/// fallback, so the request stays well-formed for a provider/tier that cannot see the image.
///
/// Delegates the per-message filter to [`crate::provider::MessagePart::strip_images`], the
/// same helper `Agent::persist_message`/`TranscriptWriter::append` use (#6305/#6307).
pub(crate) fn strip_image_parts(
    messages: &[crate::provider::Message],
) -> Vec<crate::provider::Message> {
    messages
        .iter()
        .cloned()
        .map(|mut m| {
            m.parts = crate::provider::MessagePart::strip_images(&m.parts);
            m
        })
        .collect()
}

/// Multi-provider LLM router implementing [`LlmProvider`](crate::provider::LlmProvider).
///
/// Construct with [`RouterProvider::new`] and configure a routing strategy via the
/// builder methods. All configuration is immutable after construction except for
/// runtime state (EMA statistics, Thompson distribution, bandit weights) which is
/// stored behind `Arc<Mutex<_>>` and updated on every successful call.
///
/// Cloning is cheap: [`RouterState`] and all per-strategy state are `Arc`-wrapped
/// and shared between the original and all clones — clone cost is proportional to
/// the number of `Arc` fields, not to provider count or strategy complexity.
#[derive(Debug, Clone)]
pub struct RouterProvider {
    /// Shared cross-strategy runtime signals (providers, turn counter, MAR, etc.).
    ///
    /// All fields inside are `Arc`-wrapped; clone is O(1).
    pub(crate) state: RouterState,
    status_tx: Option<StatusTx>,
    ema: Option<EmaTracker>,
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
    /// PILOT bandit state.
    bandit: Option<Arc<Mutex<BanditState>>>,
    /// Path for persisting bandit state. `None` disables persistence.
    bandit_state_path: Option<std::path::PathBuf>,
    /// Bandit routing configuration.
    bandit_config: Option<BanditRouterConfig>,
    /// Dedicated embedding provider for bandit feature vectors.
    /// When `None`, bandit falls back to Thompson/uniform on embed failure.
    bandit_embedding_provider: Option<Arc<dyn crate::provider_dyn::LlmProviderDyn>>,
    /// LRU embedding cache: maps query-string hash to feature vector.
    /// Shared across requests; keyed by `u64` hash of query text.
    bandit_embed_cache: Arc<Mutex<BanditEmbedCache>>,
    /// Agent Stability Index state (session-only coherence tracking).
    asi: Option<Arc<Mutex<AsiState>>>,
    /// ASI configuration. `None` when ASI is disabled.
    asi_config: Option<AsiRouterConfig>,
    /// Embedding-based quality gate threshold. `None` = disabled.
    /// After provider selection, `cosine_similarity(query_emb, response_emb)` must be >= this
    /// value; otherwise the next provider in the ordered list is tried.
    quality_gate: Option<f32>,
    /// `CoE` (Collaborative Entropy) router. `None` when `CoE` is disabled.
    coe: Option<Arc<CoeRouter>>,
    /// Per-call timeout for `embed()` across all non-bandit providers (milliseconds).
    /// Defaults to 5000 ms. A stalled provider is skipped and the next one is tried.
    embed_timeout_ms: u64,
    /// Bounded set of fire-and-forget ASI coherence update tasks.
    ///
    /// Shared across all clones via `Arc`; capped at [`MAX_ASI_TASKS`]. New spawns are
    /// skipped (not aborted) when the cap is reached to preserve in-flight work.
    asi_tasks: Arc<Mutex<tokio::task::JoinSet<()>>>,
}

#[cfg(test)]
mod tests;
