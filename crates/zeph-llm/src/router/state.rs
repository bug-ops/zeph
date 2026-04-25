// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared cross-strategy state for [`crate::router::RouterProvider`].
//!
//! [`RouterState`] owns all `Arc`-wrapped signals that multiple routing strategies
//! read or mutate concurrently: the provider list, turn counter, MAR confidence,
//! reputation attribution pointer, and embed-call telemetry. Grouping these here
//! separates *what is shared* from *per-strategy configuration*, which lives on
//! [`crate::router::RouterProvider`] directly.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use parking_lot::Mutex;

use crate::any::AnyProvider;
use crate::provider::LlmProvider;

/// Shared runtime signals for [`crate::router::RouterProvider`].
///
/// Every field is already `Arc`-wrapped, so cloning `RouterState` is O(1) — atomic
/// reference-count increments only. This is the same cost as the previous per-field
/// clones on `RouterProvider`, with better locality and clearer ownership.
///
/// # Cross-strategy use
///
/// | Field | Used by |
/// |---|---|
/// | `providers` / `provider_order` | all strategies |
/// | `provider_models` | Bandit (cost weight), Cascade |
/// | `last_active_provider` | RAPS reputation attribution |
/// | `last_memory_confidence` | Bandit MAR signal |
/// | `turn_counter` / `asi_last_turn` | ASI debounce |
/// | `embed_call_count` / `embed_cache_hits` | observability stats |
/// | `embed_semaphore` | embed concurrency limiter |
#[derive(Debug, Clone)]
pub struct RouterState {
    /// Ordered slice of configured backend providers.
    ///
    /// `Arc<[T]>` keeps `clone()` O(1) regardless of provider count.
    pub providers: Arc<[AnyProvider]>,

    /// Maps provider name → model identifier for cost-estimation heuristics.
    ///
    /// Built once at construction from the provider list.
    pub provider_models: Arc<HashMap<String, String>>,

    /// Current EMA-sorted provider order (indices into `providers`).
    ///
    /// Updated after every successful call; shared across clones so that concurrent
    /// sub-calls within a turn observe a consistent ordering.
    pub provider_order: Arc<Mutex<Vec<usize>>>,

    /// Name of the sub-provider that served the most recent successful tool call.
    ///
    /// Written by the router after each `chat_with_tools` dispatch; read by
    /// `record_quality_outcome` to attribute the RAPS signal to the correct provider.
    pub last_active_provider: Arc<Mutex<Option<String>>>,

    /// MAR (Memory-Augmented Routing) signal for the current turn.
    ///
    /// Set by the agent via `RouterProvider::set_memory_confidence` before each
    /// `chat` / `chat_stream` call. Read by `bandit_select_provider` to bias toward
    /// cheaper providers when memory recall confidence is high.
    pub last_memory_confidence: Arc<Mutex<Option<f32>>>,

    /// Monotonically increasing per-turn counter.
    ///
    /// Incremented once per top-level `chat()` call. Shared across clones so that
    /// concurrent sub-calls (tool schema fetches, embed probes) see the same `turn_id`,
    /// enabling ASI debounce and per-turn cache invalidation.
    pub turn_counter: Arc<AtomicU64>,

    /// Turn ID of the last ASI embedding update.
    ///
    /// Compared against `turn_counter` in `spawn_asi_update` to ensure only one
    /// embed call fires per turn even when `chat()` is invoked concurrently.
    pub asi_last_turn: Arc<AtomicU64>,

    /// Semaphore limiting concurrent `embed_batch` calls. `None` = unlimited.
    pub embed_semaphore: Option<Arc<tokio::sync::Semaphore>>,

    /// Total embed calls attempted via `embed_cached` this session.
    pub embed_call_count: Arc<AtomicU64>,

    /// Cache hits from `TurnEmbedCache` this session.
    pub embed_cache_hits: Arc<AtomicU64>,
}

impl RouterState {
    /// Build initial state from the provider list.
    ///
    /// `turn_counter` starts at 0; `asi_last_turn` starts at `u64::MAX` so the
    /// first turn always triggers an ASI update.
    pub fn new(providers: Arc<[AnyProvider]>) -> Self {
        let n = providers.len();
        let provider_models: HashMap<String, String> = providers
            .iter()
            .map(|p| (p.name().to_owned(), p.model_identifier().to_owned()))
            .collect();
        Self {
            providers,
            provider_models: Arc::new(provider_models),
            provider_order: Arc::new(Mutex::new((0..n).collect())),
            last_active_provider: Arc::new(Mutex::new(None)),
            last_memory_confidence: Arc::new(Mutex::new(None)),
            turn_counter: Arc::new(AtomicU64::new(0)),
            asi_last_turn: Arc::new(AtomicU64::new(u64::MAX)),
            embed_semaphore: None,
            embed_call_count: Arc::new(AtomicU64::new(0)),
            embed_cache_hits: Arc::new(AtomicU64::new(0)),
        }
    }
}
