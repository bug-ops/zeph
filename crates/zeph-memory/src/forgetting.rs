// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Forgetting sweep — `SleepGate` (#2397).
//!
//! Inspired by sleep-dependent memory consolidation: a background sweep periodically
//! downscales all non-consolidated message importance scores (synaptic downscaling),
//! restores recently-accessed messages (selective replay), then prunes messages whose
//! scores fall below `forgetting_floor` (targeted forgetting).
//!
//! # Algorithm
//!
//! 1. **Synaptic downscaling** — multiply all active, non-consolidated importance scores
//!    by `(1.0 - decay_rate)` in a single batch UPDATE.
//! 2. **Selective replay** — undo the current sweep's decay for messages accessed within
//!    `replay_window_hours` or with `access_count >= replay_min_access_count`.
//! 3. **Targeted forgetting** — soft-delete messages below `forgetting_floor` that are
//!    not protected by recent access or high access count.
//!
//! All three phases run inside a single `SQLite` transaction to prevent intermediate state
//! from being visible to concurrent readers (WAL readers see the pre-transaction snapshot
//! until commit).
//!
//! # Interaction with consolidation
//!
//! Forgetting only targets non-consolidated messages (`consolidated = 0`). Consolidation
//! merge transactions re-check `deleted_at IS NULL` before writing, so messages deleted
//! by forgetting are safely skipped during the next consolidation sweep.
//!
//! # No LLM calls
//!
//! Pure SQL arithmetic — no `*_provider` field needed.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::error::MemoryError;
use crate::store::SqliteStore;

pub use zeph_common::config::memory::ForgettingConfig;

// ── Result ────────────────────────────────────────────────────────────────────

/// Outcome of a single forgetting sweep.
#[derive(Debug, Default)]
pub struct ForgettingResult {
    /// Number of messages whose importance score was downscaled.
    pub downscaled: u32,
    /// Number of messages whose score was restored via selective replay.
    pub replayed: u32,
    /// Number of messages soft-deleted by targeted forgetting.
    pub pruned: u32,
}

// ── Sweep loop ────────────────────────────────────────────────────────────────

/// Start the background forgetting loop (`SleepGate`).
///
/// The loop runs every `config.sweep_interval_secs` seconds, independently of the
/// consolidation loop. Both share the same `SqliteStore` without a lock because `SQLite`
/// WAL mode handles concurrent writers safely — each sweep runs inside a single
/// transaction, so consolidation merges always see either the pre-sweep or post-sweep
/// state, never an intermediate state.
///
/// Database errors are logged but do not stop the loop.
#[must_use]
pub fn start_forgetting_loop(
    store: Arc<SqliteStore>,
    config: ForgettingConfig,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !config.enabled {
            tracing::debug!("forgetting sweep disabled (forgetting.enabled = false)");
            return;
        }

        let mut ticker = tokio::time::interval(Duration::from_secs(config.sweep_interval_secs));
        // Skip the first immediate tick to avoid running at startup.
        ticker.tick().await;

        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::debug!("forgetting loop shutting down");
                    return;
                }
                _ = ticker.tick() => {}
            }

            tracing::debug!("forgetting: starting sweep");
            let start = std::time::Instant::now();

            match run_forgetting_sweep(&store, &config).await {
                Ok(r) => {
                    tracing::info!(
                        downscaled = r.downscaled,
                        replayed = r.replayed,
                        pruned = r.pruned,
                        elapsed_ms = start.elapsed().as_millis(),
                        "forgetting: sweep complete"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        elapsed_ms = start.elapsed().as_millis(),
                        "forgetting: sweep failed, will retry"
                    );
                }
            }
        }
    })
}

// ── Sweep implementation ──────────────────────────────────────────────────────

/// Execute one full forgetting sweep (`SleepGate`).
///
/// All three phases run inside a single transaction to prevent intermediate state
/// from being visible to concurrent readers.
///
/// Returns early (no-op) if `config` contains out-of-range values, logging a warning.
/// Valid ranges:
/// - `decay_rate` in (0.0, 1.0) exclusive
/// - `forgetting_floor` in [0.0, 1.0) exclusive upper bound
/// - `sweep_interval_secs >= 60`
///
/// # Errors
///
/// Returns an error if any database operation fails.
pub async fn run_forgetting_sweep(
    store: &SqliteStore,
    config: &ForgettingConfig,
) -> Result<ForgettingResult, MemoryError> {
    if config.decay_rate <= 0.0 || config.decay_rate >= 1.0 {
        tracing::warn!(
            decay_rate = config.decay_rate,
            "forgetting: decay_rate must be in (0.0, 1.0); skipping sweep"
        );
        return Ok(ForgettingResult::default());
    }
    if config.forgetting_floor < 0.0 || config.forgetting_floor >= 1.0 {
        tracing::warn!(
            forgetting_floor = config.forgetting_floor,
            "forgetting: forgetting_floor must be in [0.0, 1.0); skipping sweep"
        );
        return Ok(ForgettingResult::default());
    }
    if config.sweep_interval_secs < 60 {
        tracing::warn!(
            sweep_interval_secs = config.sweep_interval_secs,
            "forgetting: sweep_interval_secs must be >= 60; skipping sweep"
        );
        return Ok(ForgettingResult::default());
    }
    store.run_forgetting_sweep_tx(config).await
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;
    use zeph_common::config::memory::ForgettingConfig;

    async fn make_store() -> SqliteStore {
        SqliteStore::new(":memory:")
            .await
            .expect("SqliteStore::new")
    }

    fn default_config() -> ForgettingConfig {
        ForgettingConfig {
            enabled: true,
            decay_rate: 0.1,
            forgetting_floor: 0.05,
            sweep_interval_secs: 7200,
            sweep_batch_size: 500,
            replay_window_hours: 24,
            replay_min_access_count: 3,
            protect_recent_hours: 24,
            protect_min_access_count: 3,
        }
    }

    #[tokio::test]
    async fn sweep_on_empty_db_is_noop() {
        let store = make_store().await;
        let result = run_forgetting_sweep(&store, &default_config())
            .await
            .expect("sweep");
        assert_eq!(result.downscaled, 0);
        assert_eq!(result.replayed, 0);
        assert_eq!(result.pruned, 0);
    }

    #[tokio::test]
    async fn downscaling_reduces_importance_score() {
        let store = make_store().await;
        let cid = store.create_conversation().await.expect("conversation");

        // Insert a message and set a high importance score.
        let mid = store
            .save_message(cid, "user", "hello world")
            .await
            .expect("save_message");
        store
            .set_importance_score(mid, 0.8)
            .await
            .expect("set score");

        let config = ForgettingConfig {
            decay_rate: 0.1,
            forgetting_floor: 0.01, // very low — won't prune
            protect_recent_hours: 0,
            protect_min_access_count: 999,
            replay_min_access_count: 999,
            replay_window_hours: 0,
            ..default_config()
        };

        run_forgetting_sweep(&store, &config).await.expect("sweep");

        let importance = store
            .get_importance_score(mid)
            .await
            .expect("get score")
            .expect("score exists");
        // 0.8 * (1 - 0.1) = 0.72, allow small float epsilon
        assert!(
            (importance - 0.72_f64).abs() < 1e-5,
            "expected ~0.72, got {importance}"
        );
    }

    #[tokio::test]
    async fn low_score_message_is_pruned() {
        let store = make_store().await;
        let cid = store.create_conversation().await.expect("conversation");
        let mid = store
            .save_message(cid, "user", "stale memory")
            .await
            .expect("save");
        store
            .set_importance_score(mid, 0.04)
            .await
            .expect("set score");

        let config = ForgettingConfig {
            decay_rate: 0.1,
            forgetting_floor: 0.05,
            protect_recent_hours: 0,
            protect_min_access_count: 999,
            replay_min_access_count: 999,
            replay_window_hours: 0,
            ..default_config()
        };

        let result = run_forgetting_sweep(&store, &config).await.expect("sweep");
        assert_eq!(result.pruned, 1, "low-score message must be pruned");
    }

    #[tokio::test]
    async fn high_access_message_is_protected_from_pruning() {
        let store = make_store().await;
        let cid = store.create_conversation().await.expect("conversation");
        let mid = store
            .save_message(cid, "user", "frequently accessed")
            .await
            .expect("save");
        store
            .set_importance_score(mid, 0.02)
            .await
            .expect("set score");
        // Simulate high access count via batch_increment_access_count.
        store
            .batch_increment_access_count(&[mid])
            .await
            .expect("increment");
        store
            .batch_increment_access_count(&[mid])
            .await
            .expect("increment");
        store
            .batch_increment_access_count(&[mid])
            .await
            .expect("increment");

        let config = ForgettingConfig {
            decay_rate: 0.1,
            forgetting_floor: 0.05,
            protect_recent_hours: 0,
            protect_min_access_count: 3, // protected at 3
            replay_min_access_count: 999,
            replay_window_hours: 0,
            ..default_config()
        };

        let result = run_forgetting_sweep(&store, &config).await.expect("sweep");
        assert_eq!(result.pruned, 0, "high-access message must be protected");
    }

    #[tokio::test]
    async fn recently_accessed_message_is_replayed() {
        let store = make_store().await;
        let cid = store.create_conversation().await.expect("conversation");
        let mid = store
            .save_message(cid, "user", "recently accessed memory")
            .await
            .expect("save");
        // Set a moderate importance score, then access it (sets last_accessed = now).
        store
            .set_importance_score(mid, 0.5)
            .await
            .expect("set score");
        store
            .batch_increment_access_count(&[mid])
            .await
            .expect("access");

        let config = ForgettingConfig {
            decay_rate: 0.1,
            forgetting_floor: 0.01,
            // Replay window of 1 hour catches last_accessed = now.
            replay_window_hours: 1,
            replay_min_access_count: 999, // only trigger via recency, not access count
            protect_recent_hours: 0,
            protect_min_access_count: 999,
            ..default_config()
        };

        let result = run_forgetting_sweep(&store, &config).await.expect("sweep");
        assert_eq!(
            result.replayed, 1,
            "recently accessed message must be replayed"
        );

        // Score should be back near 0.5 (decayed then restored): 0.5 * 0.9 / 0.9 = 0.5.
        let importance = store
            .get_importance_score(mid)
            .await
            .expect("get score")
            .expect("score exists");
        assert!(
            (importance - 0.5_f64).abs() < 1e-4,
            "replayed score must be restored to ~0.5, got {importance}"
        );
    }

    #[tokio::test]
    async fn consolidated_messages_are_not_downscaled() {
        let store = make_store().await;
        let cid = store.create_conversation().await.expect("conversation");
        let mid = store
            .save_message(cid, "user", "consolidated msg")
            .await
            .expect("save");
        store
            .set_importance_score(mid, 0.8)
            .await
            .expect("set score");
        store
            .mark_messages_consolidated(&[mid.0])
            .await
            .expect("mark consolidated");

        let config = ForgettingConfig {
            decay_rate: 0.1,
            forgetting_floor: 0.01,
            protect_recent_hours: 0,
            protect_min_access_count: 999,
            replay_min_access_count: 999,
            replay_window_hours: 0,
            ..default_config()
        };

        let result = run_forgetting_sweep(&store, &config).await.expect("sweep");
        // Consolidated messages must be skipped entirely.
        assert_eq!(result.downscaled, 0);
        assert_eq!(result.pruned, 0);
    }
}
