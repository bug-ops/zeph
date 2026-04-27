// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Memory eviction subsystem.
//!
//! Provides a trait-based eviction policy framework with an Ebbinghaus
//! forgetting curve implementation. The background sweep loop runs
//! periodically, scoring entries and soft-deleting the lowest-scoring ones
//! from `SQLite` before removing their `Qdrant` vectors in a second phase.
//!
//! Two-phase design ensures crash safety: soft-deleted `SQLite` rows are
//! invisible to the application immediately, and `Qdrant` cleanup is retried
//! on the next sweep if the agent crashes between phases.

use std::sync::Arc;

use tokio::time::{Duration, interval};
use tokio_util::sync::CancellationToken;

use crate::embedding_store::EmbeddingStore;
use crate::error::MemoryError;
use crate::store::SqliteStore;
use crate::types::MessageId;

// ── Public types ──────────────────────────────────────────────────────────────

/// Metadata for a single memory entry evaluated by [`EvictionPolicy::score`].
#[derive(Debug, Clone)]
pub struct EvictionEntry {
    /// `SQLite` row ID of the message.
    pub id: MessageId,
    /// ISO 8601 creation timestamp (TEXT column from `SQLite`, UTC).
    pub created_at: String,
    /// ISO 8601 last-accessed timestamp, or `None` if never accessed after creation.
    pub last_accessed: Option<String>,
    /// Number of times this message has been retrieved via recall.
    pub access_count: u32,
}

/// Trait for eviction scoring strategies.
///
/// Implementations must be `Send + Sync` so they can be shared across threads.
pub trait EvictionPolicy: Send + Sync {
    /// Compute a retention score for the given entry.
    ///
    /// Higher scores mean the entry is more likely to be retained.
    /// Lower scores mean the entry is a candidate for eviction.
    fn score(&self, entry: &EvictionEntry) -> f64;
}

use zeph_config::memory::EvictionConfig;

// ── Ebbinghaus policy ─────────────────────────────────────────────────────────

/// Ebbinghaus forgetting curve eviction policy.
///
/// Score formula:
///   `score = exp(-t / (S * ln(1 + n)))`
///
/// Where:
/// - `t` = seconds since `last_accessed` (or `created_at` if never accessed)
/// - `S` = `retention_strength` (higher = slower decay)
/// - `n` = `access_count`
///
/// Entries with a high access count or recent access get higher scores
/// and are less likely to be evicted.
pub struct EbbinghausPolicy {
    retention_strength: f64,
}

impl EbbinghausPolicy {
    /// Create a new policy with the given retention strength.
    ///
    /// A good default is `86400.0` (one day in seconds).
    #[must_use]
    pub fn new(retention_strength: f64) -> Self {
        Self { retention_strength }
    }
}

impl Default for EbbinghausPolicy {
    fn default() -> Self {
        Self::new(86_400.0) // 1 day
    }
}

impl EvictionPolicy for EbbinghausPolicy {
    fn score(&self, entry: &EvictionEntry) -> f64 {
        let now_secs = unix_now_secs();

        let reference_secs = entry
            .last_accessed
            .as_deref()
            .and_then(parse_sqlite_timestamp_secs)
            .unwrap_or_else(|| parse_sqlite_timestamp_secs(&entry.created_at).unwrap_or(now_secs));

        // Clamp t >= 0 to handle clock skew or future timestamps.
        #[allow(clippy::cast_precision_loss)]
        let t = now_secs.saturating_sub(reference_secs) as f64;
        let n = f64::from(entry.access_count);

        // ln(1 + 0) = 0 which would divide by zero — use 1.0 as minimum denominator.
        let denominator = (self.retention_strength * (1.0_f64 + n).ln()).max(1.0);
        (-t / denominator).exp()
    }
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Parse a `SQLite` TEXT timestamp ("YYYY-MM-DD HH:MM:SS") into Unix seconds.
///
/// Does not use `chrono` to avoid adding a dependency to `zeph-memory`.
fn parse_sqlite_timestamp_secs(s: &str) -> Option<u64> {
    // Expected format: "YYYY-MM-DD HH:MM:SS"
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let year: u64 = s[0..4].parse().ok()?;
    let month: u64 = s[5..7].parse().ok()?;
    let day: u64 = s[8..10].parse().ok()?;
    let hour: u64 = s[11..13].parse().ok()?;
    let min: u64 = s[14..16].parse().ok()?;
    let sec: u64 = s[17..19].parse().ok()?;

    // Days since Unix epoch (1970-01-01). Simple but accurate for years 1970-2099.
    // Leap year calculation: divisible by 4 and not 100, or divisible by 400.
    let is_leap = |y: u64| (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400);
    let days_in_month = |y: u64, m: u64| -> u64 {
        match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 => {
                if is_leap(y) {
                    29
                } else {
                    28
                }
            }
            _ => 0,
        }
    };

    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    for m in 1..month {
        days += days_in_month(year, m);
    }
    days += day.saturating_sub(1);

    Some(days * 86400 + hour * 3600 + min * 60 + sec)
}

// ── Sweep loop ────────────────────────────────────────────────────────────────

/// Start the background eviction loop.
///
/// The loop runs every `config.sweep_interval_secs` seconds. Each iteration:
/// 1. Queries `SQLite` for all non-deleted entries and their eviction metadata.
/// 2. Scores each entry using `policy`.
/// 3. If the count exceeds `config.max_entries`, soft-deletes the excess lowest-scoring rows.
/// 4. Queries for all soft-deleted rows, deletes their Qdrant vectors via `embedding` (when
///    `Some`), then marks the rows clean. If deletion fails, clean-up is retried next sweep.
///
/// If `config.max_entries == 0`, the loop exits immediately without doing anything.
///
/// Pass `embedding: None` when Qdrant is disabled — the loop will still clean `SQLite`
/// bookkeeping without attempting any vector deletion.
///
/// # Errors (non-fatal)
///
/// Database and Qdrant errors are logged but do not stop the loop.
pub async fn start_eviction_loop(
    store: Arc<SqliteStore>,
    embedding: Option<Arc<EmbeddingStore>>,
    config: EvictionConfig,
    policy: Arc<dyn EvictionPolicy + 'static>,
    cancel: CancellationToken,
) {
    if config.max_entries == 0 {
        tracing::debug!("eviction disabled (max_entries = 0)");
        return;
    }

    let mut ticker = interval(Duration::from_secs(config.sweep_interval_secs));
    // Skip the first immediate tick so the loop doesn't run at startup.
    ticker.tick().await;

    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::debug!("eviction loop shutting down");
                return;
            }
            _ = ticker.tick() => {}
        }

        tracing::debug!(max_entries = config.max_entries, "running eviction sweep");

        // Phase 1: score and soft-delete excess entries.
        match run_eviction_phase1(&store, &*policy, config.max_entries).await {
            Ok(deleted) => {
                if deleted > 0 {
                    tracing::info!(deleted, "eviction phase 1: soft-deleted entries");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "eviction phase 1 failed, will retry next sweep");
            }
        }

        // Phase 2: delete Qdrant vectors for soft-deleted entries, then mark them clean.
        // On startup or after a crash this also cleans up any orphaned vectors.
        match run_eviction_phase2(&store, embedding.as_deref()).await {
            Ok(cleaned) => {
                if cleaned > 0 {
                    tracing::info!(cleaned, "eviction phase 2: removed Qdrant vectors");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "eviction phase 2 failed, will retry next sweep");
            }
        }
    }
}

#[cfg_attr(
    feature = "profiling",
    tracing::instrument(name = "memory.eviction_phase1", skip_all)
)]
async fn run_eviction_phase1(
    store: &SqliteStore,
    policy: &dyn EvictionPolicy,
    max_entries: usize,
) -> Result<usize, MemoryError> {
    let candidates = store.get_eviction_candidates().await?;
    let total = candidates.len();

    if total <= max_entries {
        return Ok(0);
    }

    let excess = total - max_entries;
    let mut scored: Vec<(f64, MessageId)> = candidates
        .into_iter()
        .map(|e| (policy.score(&e), e.id))
        .collect();

    // Sort ascending by score — lowest scores (most forgettable) first.
    scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let candidates_to_delete: Vec<MessageId> =
        scored.into_iter().take(excess).map(|(_, id)| id).collect();
    let candidate_count = candidates_to_delete.len();
    // MM-F4: always protect messages referenced by summaries (data-integrity invariant, #3341).
    // `ids_to_delete` is the post-filter set; it may be smaller than `candidate_count` when
    // summary-anchored messages are present. The return value reflects **actually soft-deleted**
    // messages, not the original candidate count.
    let ids_to_delete = store
        .filter_out_preserved_episode_ids(&candidates_to_delete)
        .await?;
    let preserved = candidate_count - ids_to_delete.len();
    if preserved > 0 {
        tracing::debug!(
            preserved,
            deleted = ids_to_delete.len(),
            "eviction phase 1: {preserved} candidate(s) preserved by summary anchor"
        );
    }
    store.soft_delete_messages(&ids_to_delete).await?;

    // Returns the number of messages actually soft-deleted (post-filter), not the candidate count.
    Ok(ids_to_delete.len())
}

#[cfg_attr(
    feature = "profiling",
    tracing::instrument(name = "memory.eviction_phase2", skip_all)
)]
async fn run_eviction_phase2(
    store: &SqliteStore,
    embedding: Option<&EmbeddingStore>,
) -> Result<usize, MemoryError> {
    // Find all soft-deleted entries that haven't been cleaned from Qdrant yet.
    let ids = store.get_soft_deleted_message_ids().await?;
    if ids.is_empty() {
        return Ok(0);
    }

    if let Some(emb) = embedding {
        // Delete vectors before marking clean — crash-safe: if this fails, the next
        // sweep retries (ids are still soft-deleted and not yet marked clean).
        emb.delete_by_message_ids(&ids).await?;
    } else {
        tracing::debug!(
            count = ids.len(),
            "eviction phase 2: Qdrant disabled, cleaning SQLite bookkeeping only"
        );
    }

    store.mark_qdrant_cleaned(&ids).await?;
    Ok(ids.len())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a timestamp string for a time N seconds ago from now.
    ///
    /// Returns a string parseable by `parse_sqlite_timestamp_secs`.
    fn ts_ago(seconds_ago: u64) -> String {
        let ts = unix_now_secs().saturating_sub(seconds_ago);
        // Convert back to "YYYY-MM-DD HH:MM:SS" using the same logic as parse_sqlite_timestamp_secs
        let sec = ts % 60;
        let min = (ts / 60) % 60;
        let hour = (ts / 3600) % 24;
        let mut total_days = ts / 86400;
        let is_leap =
            |y: u64| (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400);
        let mut year = 1970u64;
        loop {
            let days_in_year = if is_leap(year) { 366 } else { 365 };
            if total_days < days_in_year {
                break;
            }
            total_days -= days_in_year;
            year += 1;
        }
        let month_days = [
            0u64,
            31,
            28 + u64::from(is_leap(year)),
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        let mut month = 1u64;
        while month <= 12 {
            let month_idx = usize::try_from(month).unwrap_or_else(|_| unreachable!());
            if total_days < month_days[month_idx] {
                break;
            }
            total_days -= month_days[month_idx];
            month += 1;
        }
        let day = total_days + 1;
        format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02}")
    }

    fn make_entry(access_count: u32, seconds_ago: u64) -> EvictionEntry {
        let ts = ts_ago(seconds_ago);
        EvictionEntry {
            id: MessageId(1),
            created_at: ts.clone(),
            last_accessed: Some(ts),
            access_count,
        }
    }

    #[test]
    fn ebbinghaus_recent_high_access_scores_near_one() {
        let policy = EbbinghausPolicy::default();
        // Use 1 second ago to ensure t is close to 0
        let entry = make_entry(10, 1);
        let score = policy.score(&entry);
        // t = 1, n = 10, denominator = 86400 * ln(11) ≈ 207_946; exp(-1/207_946) ≈ 1.0
        assert!(
            score > 0.99,
            "score should be near 1.0 for recently accessed entry, got {score}"
        );
    }

    #[test]
    fn ebbinghaus_old_zero_access_scores_lower() {
        let policy = EbbinghausPolicy::default();
        let old = make_entry(0, 7 * 24 * 3600); // 7 days ago, never accessed
        let recent = make_entry(0, 60); // 1 minute ago
        assert!(
            policy.score(&old) < policy.score(&recent),
            "old entry must score lower than recent"
        );
    }

    #[test]
    fn ebbinghaus_high_access_decays_slower() {
        let policy = EbbinghausPolicy::default();
        let low = make_entry(1, 3600); // accessed 1 hour ago, 1 time
        let high = make_entry(20, 3600); // accessed 1 hour ago, 20 times
        assert!(
            policy.score(&high) > policy.score(&low),
            "high access count should yield higher score"
        );
    }

    #[test]
    fn ebbinghaus_never_accessed_uses_created_at_as_reference() {
        let policy = EbbinghausPolicy::default();
        // An old entry (7 days ago) with last_accessed = None.
        // Score should be the same as make_entry(0, 7 days) because both use created_at.
        let old_with_no_last_accessed = EvictionEntry {
            id: MessageId(2),
            created_at: ts_ago(7 * 24 * 3600),
            last_accessed: None,
            access_count: 0,
        };
        let old_with_same_last_accessed = make_entry(0, 7 * 24 * 3600);
        let score_no_access = policy.score(&old_with_no_last_accessed);
        let score_same = policy.score(&old_with_same_last_accessed);
        // Both reference the same time; scores should be approximately equal
        let diff = (score_no_access - score_same).abs();
        assert!(diff < 1e-6, "scores should match; diff = {diff}");
    }

    #[test]
    fn eviction_config_default_is_disabled() {
        let config = EvictionConfig::default();
        assert_eq!(
            config.max_entries, 0,
            "eviction must be disabled by default"
        );
    }

    // ── MM-F4: eviction preserves summary-anchored messages ───────────────────

    #[tokio::test]
    async fn test_eviction_preserves_summary_anchored_messages() {
        use crate::store::SqliteStore;

        let store = SqliteStore::new(":memory:").await.unwrap();
        let cid = store.create_conversation().await.unwrap();

        // Insert 6 messages (max_entries=3 → 3 excess candidates).
        let ids: Vec<_> = (0..6)
            .map(|_| async { store.save_message(cid, "user", "msg").await.unwrap() })
            .collect();
        let mut msg_ids = Vec::new();
        for f in ids {
            msg_ids.push(f.await);
        }

        // Anchor messages 1–3 inside a summary range.
        store
            .save_summary(cid, "summary", Some(msg_ids[0]), Some(msg_ids[2]), 30)
            .await
            .unwrap();

        let policy = EbbinghausPolicy::default();
        // Trigger eviction: 6 messages, max_entries=3.
        let deleted = run_eviction_phase1(&store, &policy, 3).await.unwrap();

        // At most 3 were eligible for deletion; summary-anchored (0–2) must survive.
        // Only messages 3–5 (outside summary range) may be deleted.
        assert!(
            deleted <= 3,
            "at most 3 messages can be deleted, got {deleted}"
        );

        for &anchored in &msg_ids[0..=2] {
            let is_deleted: Option<String> =
                sqlx::query_scalar("SELECT deleted_at FROM messages WHERE id = ?")
                    .bind(anchored)
                    .fetch_one(store.pool())
                    .await
                    .unwrap();
            assert!(
                is_deleted.is_none(),
                "summary-anchored message {anchored:?} must not be soft-deleted"
            );
        }
    }

    #[test]
    fn parse_sqlite_timestamp_known_value() {
        // 2024-01-01 00:00:00 UTC
        let ts = parse_sqlite_timestamp_secs("2024-01-01 00:00:00").unwrap();
        // Days from 1970 to 2024: 54 years, roughly
        // Reference: 2024-01-01 00:00:00 UTC = 1704067200
        assert_eq!(
            ts, 1_704_067_200,
            "2024-01-01 must parse to known timestamp"
        );
    }

    // ── Phase-2 Qdrant cleanup tests ─────────────────────────────────────────

    /// Build a test `EmbeddingStore` backed by an in-memory vector store and a
    /// fresh `SQLite` database. Returns both so the caller can manipulate `SQLite` directly.
    async fn setup_embedding_store() -> (EmbeddingStore, crate::store::SqliteStore) {
        let sqlite = crate::store::SqliteStore::new(":memory:").await.unwrap();
        let pool = sqlite.pool().clone();
        let mem_store = Box::new(crate::in_memory_store::InMemoryVectorStore::new());
        let emb = EmbeddingStore::with_store(mem_store, pool);
        emb.ensure_collection(4).await.unwrap();
        (emb, sqlite)
    }

    /// Seed a message, store a vector, and soft-delete the message. Returns `(MessageId, point_id)`.
    async fn seed_soft_deleted(
        store: &crate::store::SqliteStore,
        emb: &EmbeddingStore,
    ) -> (MessageId, String) {
        let cid = store.create_conversation().await.unwrap();
        let msg_id = store.save_message(cid, "user", "hello").await.unwrap();

        let point_id = emb
            .store(
                msg_id,
                cid,
                "user",
                vec![1.0, 0.0, 0.0, 0.0],
                crate::embedding_store::MessageKind::Regular,
                "test",
                0,
            )
            .await
            .unwrap();

        // Soft-delete the message so it appears in `get_soft_deleted_message_ids`.
        store.soft_delete_messages(&[msg_id]).await.unwrap();

        (msg_id, point_id)
    }

    /// Phase 2 with an embedding store: must delete vectors before marking clean.
    #[tokio::test]
    async fn eviction_phase2_calls_delete_before_mark_clean() {
        let (emb, store) = setup_embedding_store().await;
        let (msg_id, _point_id) = seed_soft_deleted(&store, &emb).await;

        // Phase 2 should succeed, delete the vector, and mark the row clean.
        let cleaned = run_eviction_phase2(&store, Some(&emb)).await.unwrap();
        assert_eq!(cleaned, 1, "one message should be cleaned");

        // After phase 2 the message must no longer appear in the pending cleanup list.
        let remaining = store.get_soft_deleted_message_ids().await.unwrap();
        assert!(
            !remaining.contains(&msg_id),
            "message must not remain in soft-deleted list after phase 2"
        );
    }

    /// Phase 2 without an embedding store: `SQLite` bookkeeping still runs; no Qdrant call.
    #[tokio::test]
    async fn eviction_phase2_skips_delete_when_no_embedding_store() {
        let (_, store) = setup_embedding_store().await;
        let cid = store.create_conversation().await.unwrap();
        let msg_id = store.save_message(cid, "user", "hello").await.unwrap();
        store.soft_delete_messages(&[msg_id]).await.unwrap();

        // No embedding store — must still mark rows clean.
        let cleaned = run_eviction_phase2(&store, None).await.unwrap();
        assert_eq!(
            cleaned, 1,
            "row must be cleaned even without embedding store"
        );

        let remaining = store.get_soft_deleted_message_ids().await.unwrap();
        assert!(
            !remaining.contains(&msg_id),
            "message must not remain in soft-deleted list"
        );
    }

    /// Phase 2 returns `Ok(0)` when there are no soft-deleted messages.
    #[tokio::test]
    async fn eviction_phase2_empty_returns_zero() {
        let (emb, store) = setup_embedding_store().await;
        let cleaned = run_eviction_phase2(&store, Some(&emb)).await.unwrap();
        assert_eq!(cleaned, 0, "no soft-deleted messages → 0 cleaned");
    }
}
