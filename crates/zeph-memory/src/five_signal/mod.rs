// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Five-signal SYNAPSE retrieval subsystem (issue #4374).
//!
//! Extends the SYNAPSE recall pipeline with three additional signals beyond the
//! two-signal baseline (recency + relevance):
//!
//! - **Access frequency** — facts queried more often rank higher.
//! - **Causal distance** — facts causally closer to the current goal rank higher.
//! - **Novelty** — facts created early in the session rank higher than late-session facts.
//!
//! When all new signal weights are `0.0` (the default), the five-signal formula is
//! algebraically equivalent to the existing two-signal baseline.

pub mod access_frequency;
pub mod causal_distance;
pub mod consolidation;
pub mod metrics;
pub mod novelty;
pub mod scoring;
pub mod weights;

use std::sync::Arc;

use zeph_common::SessionId;
use zeph_config::memory::FiveSignalConfig;
use zeph_db::DbPool;

use crate::embedding_store::EmbeddingStore;
use crate::five_signal::{
    access_frequency::AccessFrequencyCache, causal_distance::CausalDistanceComputer,
    metrics::FiveSignalMetrics, novelty::NoveltyComputer, weights::FiveSignalWeights,
};

/// Runtime state for the five-signal retrieval subsystem.
///
/// Created once at bootstrap when `five_signal.enabled = true` and attached to
/// [`crate::semantic::SemanticMemory`] via an `Option<Arc<FiveSignalRuntime>>`.
/// `None` when disabled — guarantees zero overhead per NFR-005.
pub struct FiveSignalRuntime {
    /// Normalized signal weights (computed once at startup).
    pub weights: FiveSignalWeights,
    /// Access frequency aggregator.
    pub access_cache: AccessFrequencyCache,
    /// Causal distance computer (contains BFS cache per goal entity).
    pub causal_computer: tokio::sync::Mutex<CausalDistanceComputer>,
    /// Novelty computer (pure arithmetic, no I/O).
    pub novelty_computer: NoveltyComputer,
    /// Prometheus-compatible counters.
    pub metrics: Arc<FiveSignalMetrics>,
    /// `SQLite` pool (shared with the rest of `SemanticMemory`).
    pub pool: DbPool,
    /// Qdrant store (optional; used by the consolidation daemon).
    pub qdrant: Option<Arc<EmbeddingStore>>,
    /// Unix timestamp of session start, used by `NoveltyComputer`.
    pub session_start: i64,
    /// Session identifier used to scope `fact_access_log` inserts and queries.
    ///
    /// Set at bootstrap from a per-process UUID so access counts are isolated
    /// per session and do not bleed across process restarts.
    pub session_id: SessionId,
    /// Config snapshot (used by the consolidation daemon).
    pub config: FiveSignalConfig,
}

impl FiveSignalRuntime {
    /// Create a new runtime from config, pool, and optional graph + Qdrant stores.
    ///
    /// Normalizes signal weights (logging `WARN` if they do not sum to `1.0`).
    /// Logs a `WARN` if `consolidation_daemon.top_k_per_run < batch_size` (MINOR-03).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use zeph_config::memory::FiveSignalConfig;
    /// use zeph_memory::five_signal::FiveSignalRuntime;
    ///
    /// # async fn example(pool: zeph_db::DbPool, graph: Arc<zeph_memory::graph::GraphStore>) {
    /// let cfg = FiveSignalConfig::default();
    /// let session_start = std::time::SystemTime::now()
    ///     .duration_since(std::time::UNIX_EPOCH)
    ///     .map_or(0, |d| d.as_secs() as i64);
    /// let session_id = uuid::Uuid::new_v4().to_string();
    /// let runtime = FiveSignalRuntime::new(cfg, pool, graph, None, session_start, session_id);
    /// # }
    /// ```
    #[must_use]
    pub fn new(
        config: FiveSignalConfig,
        pool: DbPool,
        graph_store: Arc<crate::graph::GraphStore>,
        qdrant: Option<Arc<EmbeddingStore>>,
        session_start: i64,
        session_id: impl Into<SessionId>,
    ) -> Self {
        let weights = FiveSignalWeights::normalized(&config);

        // MINOR-03: enforce top_k_per_run >= batch_size at startup.
        let daemon = &config.consolidation_daemon;
        if daemon.enabled && daemon.top_k_per_run < daemon.batch_size {
            tracing::warn!(
                top_k_per_run = daemon.top_k_per_run,
                batch_size = daemon.batch_size,
                "five_signal: top_k_per_run < batch_size; daemon will only process top_k_per_run facts"
            );
        }

        Self {
            weights,
            access_cache: AccessFrequencyCache::new(pool.clone()),
            causal_computer: tokio::sync::Mutex::new(CausalDistanceComputer::new(
                graph_store,
                config.causal_bfs_max_depth,
                config.neutral_causal_distance,
            )),
            novelty_computer: NoveltyComputer::new(session_start, config.novelty_decay_rate),
            metrics: Arc::new(FiveSignalMetrics::default()),
            pool,
            qdrant,
            session_start,
            session_id: session_id.into(),
            config,
        }
    }
}
