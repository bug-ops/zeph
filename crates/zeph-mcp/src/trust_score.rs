// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::time::{Duration, SystemTime, UNIX_EPOCH};
#[allow(unused_imports)]
use zeph_db::sql;

use serde::{Deserialize, Serialize};
use zeph_db::DbPool;

use crate::manager::McpTrustLevel;

/// Persistent per-server trust score with asymmetric time decay.
///
/// The score lives in `[0.0, 1.0]` and starts at `INITIAL_SCORE` (0.5 = neutral).
/// Successful tool calls boost it by `SUCCESS_BOOST`; failures reduce it by
/// `FAILURE_PENALTY`; injection detections reduce it by `INJECTION_PENALTY`.
/// Only scores **above** `INITIAL_SCORE` decay over time — low-scoring servers must
/// earn trust back through successful calls, not by waiting.
///
/// Use [`recommended_trust_level`](Self::recommended_trust_level) to map the
/// current score to a [`McpTrustLevel`] for runtime gating decisions.
///
/// Scores are persisted via [`TrustScoreStore`] so they survive agent restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerTrustScore {
    /// Unique server identifier (matches [`ServerEntry::id`](crate::manager::ServerEntry)).
    pub server_id: String,
    /// Cumulative trust score in `[0.0, 1.0]`. `0.5` = neutral (initial value).
    pub score: f64,
    /// Number of successful tool calls recorded.
    pub success_count: u64,
    /// Number of failed or injection-detected calls recorded.
    pub failure_count: u64,
    /// Unix timestamp of the last update.
    pub updated_at_secs: u64,
}

impl ServerTrustScore {
    pub const INITIAL_SCORE: f64 = 0.5;
    /// Per-day decay applied only to scores above `INITIAL_SCORE`.
    pub const DECAY_RATE: f64 = 0.01;
    pub const SUCCESS_BOOST: f64 = 0.02;
    pub const FAILURE_PENALTY: f64 = 0.10;
    pub const INJECTION_PENALTY: f64 = 0.25;

    /// Create a new `ServerTrustScore` at the neutral initial score (0.5).
    #[must_use]
    pub fn new(server_id: impl Into<String>) -> Self {
        Self {
            server_id: server_id.into(),
            score: Self::INITIAL_SCORE,
            success_count: 0,
            failure_count: 0,
            updated_at_secs: unix_now(),
        }
    }

    fn days_since_update(&self) -> f64 {
        let now = unix_now();
        let delta = now.saturating_sub(self.updated_at_secs);
        Duration::from_secs(delta).as_secs_f64() / 86_400.0
    }

    /// Asymmetric decay: only scores above 0.5 decay toward 0.5.
    /// Scores at or below 0.5 require explicit `record_success()` calls to recover.
    pub fn apply_decay(&mut self) {
        if self.score > Self::INITIAL_SCORE {
            let days = self.days_since_update();
            let decay = Self::DECAY_RATE * days;
            self.score = (self.score - decay).max(Self::INITIAL_SCORE);
        }
        self.updated_at_secs = unix_now();
    }

    /// Increase the score by `SUCCESS_BOOST` (capped at 1.0) and increment `success_count`.
    pub fn record_success(&mut self) {
        self.score = (self.score + Self::SUCCESS_BOOST).min(1.0);
        self.success_count += 1;
        self.updated_at_secs = unix_now();
    }

    /// Decrease the score by `FAILURE_PENALTY` (floored at 0.0) and increment `failure_count`.
    pub fn record_failure(&mut self) {
        self.score = (self.score - Self::FAILURE_PENALTY).max(0.0);
        self.failure_count += 1;
        self.updated_at_secs = unix_now();
    }

    /// Decrease the score by `INJECTION_PENALTY` (floored at 0.0) and increment `failure_count`.
    ///
    /// Applied when the prober or embedding guard detects an injection pattern in server output.
    pub fn record_injection(&mut self) {
        self.score = (self.score - Self::INJECTION_PENALTY).max(0.0);
        self.failure_count += 1;
        self.updated_at_secs = unix_now();
    }

    /// Recommend a trust level based on current score.
    #[must_use]
    pub fn recommended_trust_level(&self) -> McpTrustLevel {
        if self.score >= 0.8 {
            McpTrustLevel::Trusted
        } else if self.score >= 0.4 {
            McpTrustLevel::Untrusted
        } else {
            McpTrustLevel::Sandboxed
        }
    }
}

/// SQLite-backed store for per-server trust scores.
pub struct TrustScoreStore {
    pool: DbPool,
}

impl TrustScoreStore {
    /// Create a new store backed by the given pool.
    #[must_use]
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Run all pending migrations on the underlying pool.
    ///
    /// Replaces the former inline `CREATE TABLE IF NOT EXISTS` DDL. The
    /// `mcp_trust_scores` schema is now managed by migration
    /// `052_mcp_trust_scores.sql` in `zeph-db`.
    ///
    /// # Errors
    ///
    /// Returns an error if any migration fails.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "mcp.trust_score.init", skip_all)
    )]
    pub async fn init(&self) -> Result<(), zeph_db::DbError> {
        zeph_db::run_migrations(&self.pool).await?;
        Ok(())
    }

    /// Load the trust score for a server, applying asymmetric decay at read time.
    ///
    /// Decay is applied and, when non-zero, written back to the database so that
    /// subsequent `load_and_apply_delta()` calls operate on the true current (decayed)
    /// value rather than the stale stored score. Without this write-back, a success delta
    /// would be added to the pre-decay score, effectively reversing the decay.
    ///
    /// Concurrent loads for the same server are safe: linear decay is idempotent
    /// over a given time window, so two concurrent writes produce the same value.
    ///
    /// Returns `None` if not found.
    ///
    /// # Errors
    ///
    /// Returns an error if any SQL query fails.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "mcp.trust_score.load", skip(self), fields(server_id))
    )]
    pub async fn load(
        &self,
        server_id: &str,
    ) -> Result<Option<ServerTrustScore>, zeph_db::SqlxError> {
        // success_count/failure_count are INTEGER (INT4) in the Postgres schema
        // (migration 052_mcp_trust_scores.sql); sqlx-postgres rejects decoding INT4
        // directly into i64 (`ColumnDecode`), so they must be read as i32.
        let row: Option<(String, f64, i32, i32, i64)> = zeph_db::query_as(sql!(
            "SELECT server_id, score, success_count, failure_count, updated_at_secs
             FROM mcp_trust_scores WHERE server_id = ?"
        ))
        .bind(server_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some((sid, score, sc, fc, ts)) = row else {
            return Ok(None);
        };

        let mut entry = ServerTrustScore {
            server_id: sid,
            score,
            success_count: u64::try_from(sc).unwrap_or(0),
            failure_count: u64::try_from(fc).unwrap_or(0),
            updated_at_secs: u64::try_from(ts).unwrap_or(0),
        };

        let score_before = entry.score;
        entry.apply_decay();

        if (entry.score - score_before).abs() > f64::EPSILON {
            let now = i64::try_from(entry.updated_at_secs).unwrap_or(i64::MAX);
            zeph_db::query(sql!(
                "UPDATE mcp_trust_scores SET score = ?, updated_at_secs = ? WHERE server_id = ?"
            ))
            .bind(entry.score)
            .bind(now)
            .bind(server_id)
            .execute(&self.pool)
            .await?;
        }

        Ok(Some(entry))
    }

    /// Atomically apply a decay-adjusted score delta and update counters.
    ///
    /// Replaces the former `apply_delta()` (decay-blind but atomic) and the former
    /// `load_and_apply_delta()` (decay-aware but a non-atomic read-then-write, vulnerable to
    /// a lost-update race between two concurrent callers for the same `server_id`). This
    /// method folds both properties into a single `INSERT ... ON CONFLICT DO UPDATE`
    /// statement: the asymmetric time-decay (see [`ServerTrustScore::apply_decay`]) is
    /// recomputed from the stored `score`/`updated_at_secs` entirely inside the SQL
    /// expression, so the whole read-decay-delta-clamp-write sequence is one atomic
    /// row-level operation — no other writer can observe or clobber an intermediate state.
    ///
    /// Behavior mirrors the old two-step version exactly: decay is applied first (only to
    /// scores above [`ServerTrustScore::INITIAL_SCORE`], floored at `INITIAL_SCORE`), then
    /// `score_delta` is added, then the result is clamped to `[0.0, 1.0]`. Elapsed time is
    /// floored at zero to guard against a clock going backward inflating the score.
    ///
    /// This equivalence holds for `|score_delta| <= 0.5`: the delta is recovered inside the
    /// SQL as `excluded.score - INITIAL_SCORE`, where `excluded.score` is bound as
    /// `(INITIAL_SCORE + score_delta).clamp(0.0, 1.0)`. If `score_delta` pushed
    /// `INITIAL_SCORE + score_delta` outside `[0.0, 1.0]`, the bound value — and therefore
    /// the recovered delta — would be clamped before decay/delta application, silently
    /// applying a smaller-magnitude delta than requested. All current callers stay well
    /// within this bound (`INJECTION_PENALTY` = 0.25, `FAILURE_PENALTY` = 0.10,
    /// `SUCCESS_BOOST` = 0.02), so this is not reachable in practice, but a future caller
    /// passing a larger delta must keep `|score_delta| <= 0.5`.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL execution fails.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(
            name = "mcp.trust_score.load_and_apply_delta",
            skip(self),
            fields(server_id)
        )
    )]
    pub async fn load_and_apply_delta(
        &self,
        server_id: &str,
        score_delta: f64,
        success_increment: u64,
        failure_increment: u64,
    ) -> Result<(), zeph_db::SqlxError> {
        let now = i64::try_from(unix_now()).unwrap_or(i64::MAX);
        // `MIN`/`MAX` are scalar multi-argument functions on SQLite but aggregate-only on
        // Postgres (no `max(numeric, double precision)` overload exists there); the
        // dialect-specific `LEAST`/`GREATEST` scalar equivalents are required instead.
        let least_fn = <zeph_db::ActiveDialect as zeph_db::Dialect>::LEAST_FN;
        let greatest_fn = <zeph_db::ActiveDialect as zeph_db::Dialect>::GREATEST_FN;
        let initial = ServerTrustScore::INITIAL_SCORE;
        let decay_rate = ServerTrustScore::DECAY_RATE;
        // `excluded.score` carries `INITIAL_SCORE + score_delta` (bound below), so
        // `excluded.score - {initial}` recovers the pure delta — same trick `apply_delta`
        // used to reuse a single bound "score" column for both the fresh-insert value and
        // the delta applied on conflict.
        //
        // Decay is computed from the pre-update row (`mcp_trust_scores.score` /
        // `.updated_at_secs`, unqualified references to the existing row inside an
        // `ON CONFLICT DO UPDATE` clause) against `excluded.updated_at_secs` (`now`, bound
        // below). `{greatest_fn}(0, ...)` floors elapsed seconds at zero so a backward clock
        // cannot produce negative "elapsed days" and inflate the score.
        let raw = format!(
            "INSERT INTO mcp_trust_scores
                (server_id, score, success_count, failure_count, updated_at_secs)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(server_id) DO UPDATE SET
                score = {least_fn}(1.0, {greatest_fn}(0.0,
                    (CASE WHEN mcp_trust_scores.score > {initial}
                          THEN {greatest_fn}({initial}, mcp_trust_scores.score - {decay_rate} * (
                              CAST({greatest_fn}(0, excluded.updated_at_secs - mcp_trust_scores.updated_at_secs) AS REAL)
                              / 86400
                          ))
                          ELSE mcp_trust_scores.score
                     END) + (excluded.score - {initial})
                )),
                success_count   = mcp_trust_scores.success_count + excluded.success_count,
                failure_count   = mcp_trust_scores.failure_count + excluded.failure_count,
                updated_at_secs = excluded.updated_at_secs"
        );
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        zeph_db::query(zeph_db::sqlx::AssertSqlSafe(query_sql))
            .bind(server_id)
            // Fresh-insert value AND the carrier for the delta on conflict (see comment above).
            .bind((ServerTrustScore::INITIAL_SCORE + score_delta).clamp(0.0, 1.0))
            .bind(i64::try_from(success_increment).unwrap_or(i64::MAX))
            .bind(i64::try_from(failure_increment).unwrap_or(i64::MAX))
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Load all server trust scores with decay applied for display accuracy.
    ///
    /// Decay is applied in-memory but NOT persisted. This is intentional: persisting
    /// decay for every row in a bulk read would generate N writes, degrading performance
    /// on large deployments. Decision-path code must always go through `load()`, which
    /// persists the decayed score so `load_and_apply_delta()` operates on the correct
    /// base value.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL query fails.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "mcp.trust_score.load_all", skip_all)
    )]
    pub async fn load_all(&self) -> Result<Vec<ServerTrustScore>, zeph_db::SqlxError> {
        // success_count/failure_count are INTEGER (INT4) in the Postgres schema
        // (migration 052_mcp_trust_scores.sql); sqlx-postgres rejects decoding INT4
        // directly into i64 (`ColumnDecode`), so they must be read as i32 (see `load()`).
        let rows: Vec<(String, f64, i32, i32, i64)> = zeph_db::query_as(sql!(
            "SELECT server_id, score, success_count, failure_count, updated_at_secs
             FROM mcp_trust_scores"
        ))
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(sid, score, sc, fc, ts)| {
                let mut entry = ServerTrustScore {
                    server_id: sid,
                    score,
                    success_count: u64::try_from(sc).unwrap_or(0),
                    failure_count: u64::try_from(fc).unwrap_or(0),
                    updated_at_secs: u64::try_from(ts).unwrap_or(0),
                };
                // Decay applied for display accuracy; not persisted (load() persists on read).
                entry.apply_decay();
                entry
            })
            .collect())
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use zeph_db::DbPool;

    async fn test_pool() -> DbPool {
        zeph_db::DbConfig {
            url: ":memory:".to_string(),
            max_connections: 5,
            pool_size: 5,
        }
        .connect()
        .await
        .unwrap()
    }

    #[test]
    fn initial_score_is_neutral() {
        let s = ServerTrustScore::new("srv");
        assert!((s.score - ServerTrustScore::INITIAL_SCORE).abs() < f64::EPSILON);
    }

    #[test]
    fn record_success_increases_score() {
        let mut s = ServerTrustScore::new("srv");
        s.record_success();
        assert!(s.score > ServerTrustScore::INITIAL_SCORE);
        assert_eq!(s.success_count, 1);
    }

    #[test]
    fn record_failure_decreases_score() {
        let mut s = ServerTrustScore::new("srv");
        s.record_failure();
        assert!(s.score < ServerTrustScore::INITIAL_SCORE);
        assert_eq!(s.failure_count, 1);
    }

    #[test]
    fn record_injection_decreases_score_more() {
        let mut s = ServerTrustScore::new("srv");
        let before = s.score;
        s.record_injection();
        assert!(s.score < before - ServerTrustScore::FAILURE_PENALTY);
    }

    #[test]
    fn score_clamped_at_zero_on_repeated_failures() {
        let mut s = ServerTrustScore::new("srv");
        for _ in 0..20 {
            s.record_failure();
        }
        assert!(s.score >= 0.0);
    }

    #[test]
    fn score_clamped_at_one_on_repeated_successes() {
        let mut s = ServerTrustScore::new("srv");
        for _ in 0..100 {
            s.record_success();
        }
        assert!(s.score <= 1.0);
    }

    #[test]
    fn asymmetric_decay_above_initial() {
        let mut s = ServerTrustScore::new("srv");
        s.score = 0.9;
        // Simulate 10 days ago.
        s.updated_at_secs = unix_now().saturating_sub(10 * 86_400);
        let before = s.score;
        s.apply_decay();
        // Score should have decreased toward 0.5.
        assert!(s.score < before);
        assert!(s.score >= ServerTrustScore::INITIAL_SCORE);
    }

    #[test]
    fn asymmetric_decay_below_initial_no_change() {
        let mut s = ServerTrustScore::new("srv");
        s.score = 0.2;
        s.updated_at_secs = unix_now().saturating_sub(100 * 86_400);
        s.apply_decay();
        // Score should NOT increase — stays at 0.2.
        assert!((s.score - 0.2).abs() < f64::EPSILON);
    }

    #[test]
    fn recommended_trust_level_trusted() {
        let mut s = ServerTrustScore::new("srv");
        s.score = 0.85;
        assert_eq!(s.recommended_trust_level(), McpTrustLevel::Trusted);
    }

    #[test]
    fn recommended_trust_level_untrusted() {
        let mut s = ServerTrustScore::new("srv");
        s.score = 0.5;
        assert_eq!(s.recommended_trust_level(), McpTrustLevel::Untrusted);
    }

    #[test]
    fn recommended_trust_level_sandboxed() {
        let mut s = ServerTrustScore::new("srv");
        s.score = 0.1;
        assert_eq!(s.recommended_trust_level(), McpTrustLevel::Sandboxed);
    }

    #[tokio::test]
    async fn store_init_and_roundtrip() {
        let pool = test_pool().await;
        let store = TrustScoreStore::new(pool);
        store.init().await.unwrap();

        // Initially no record.
        assert!(store.load("srv1").await.unwrap().is_none());

        // Apply a success delta.
        store
            .load_and_apply_delta("srv1", 0.02, 1, 0)
            .await
            .unwrap();

        let loaded = store.load("srv1").await.unwrap().unwrap();
        assert_eq!(loaded.server_id, "srv1");
        assert!(loaded.score > ServerTrustScore::INITIAL_SCORE);
        assert_eq!(loaded.success_count, 1);
        assert_eq!(loaded.failure_count, 0);
    }

    #[tokio::test]
    async fn store_load_and_apply_delta_failure() {
        let pool = test_pool().await;
        let store = TrustScoreStore::new(pool);
        store.init().await.unwrap();

        store
            .load_and_apply_delta("srv1", -ServerTrustScore::FAILURE_PENALTY, 0, 1)
            .await
            .unwrap();

        let loaded = store.load("srv1").await.unwrap().unwrap();
        assert!(loaded.score < ServerTrustScore::INITIAL_SCORE);
        assert_eq!(loaded.failure_count, 1);
    }

    #[tokio::test]
    async fn store_load_all_returns_all_servers() {
        let pool = test_pool().await;
        let store = TrustScoreStore::new(pool);
        store.init().await.unwrap();

        store.load_and_apply_delta("srv1", 0.0, 1, 0).await.unwrap();
        store.load_and_apply_delta("srv2", 0.0, 0, 1).await.unwrap();

        let all = store.load_all().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn store_atomic_update_does_not_reset() {
        let pool = test_pool().await;
        let store = TrustScoreStore::new(pool);
        store.init().await.unwrap();

        // Two consecutive success deltas.
        store
            .load_and_apply_delta("srv1", 0.02, 1, 0)
            .await
            .unwrap();
        store
            .load_and_apply_delta("srv1", 0.02, 1, 0)
            .await
            .unwrap();

        let loaded = store.load("srv1").await.unwrap().unwrap();
        assert_eq!(loaded.success_count, 2);
    }

    #[tokio::test]
    async fn store_score_clamped_between_zero_and_one() {
        let pool = test_pool().await;
        let store = TrustScoreStore::new(pool);
        store.init().await.unwrap();

        // Many large positive deltas — score must not exceed 1.0
        for _ in 0..50 {
            store.load_and_apply_delta("srv1", 0.5, 1, 0).await.unwrap();
        }
        let high = store.load("srv1").await.unwrap().unwrap();
        assert!(
            high.score <= 1.0,
            "score must not exceed 1.0, got {}",
            high.score
        );

        // Many large negative deltas — score must not go below 0.0
        for _ in 0..50 {
            store
                .load_and_apply_delta("srv2", -0.5, 0, 1)
                .await
                .unwrap();
        }
        let low = store.load("srv2").await.unwrap().unwrap();
        assert!(
            low.score >= 0.0,
            "score must not go below 0.0, got {}",
            low.score
        );
    }

    #[tokio::test]
    async fn store_load_before_any_write_returns_none() {
        // DbConfig::connect() already runs migrations, so the schema is present.
        // load() on a fresh pool with no rows should return Ok(None).
        let pool = test_pool().await;
        let store = TrustScoreStore::new(pool);
        // Do NOT call store.init() — migrations already ran via DbConfig::connect()
        let result = store.load("srv1").await;
        assert!(result.is_ok(), "load on fresh db should not error");
        assert!(result.unwrap().is_none(), "no entry should exist yet");
    }

    #[tokio::test]
    async fn store_load_persists_decay() {
        let pool = test_pool().await;
        let store = TrustScoreStore::new(pool.clone());
        store.init().await.unwrap();

        // Insert a score above INITIAL_SCORE with a timestamp 10 days in the past.
        let old_ts = unix_now().saturating_sub(10 * 86_400);
        zeph_db::query(
            sql!("INSERT INTO mcp_trust_scores (server_id, score, success_count, failure_count, updated_at_secs)
             VALUES (?, ?, 0, 0, ?)"),
        )
        .bind("srv1")
        .bind(0.9_f64)
        .bind(i64::try_from(old_ts).unwrap_or(i64::MAX))
        .execute(&pool)
        .await
        .unwrap();

        // First load: applies and persists decay.
        let first = store.load("srv1").await.unwrap().unwrap();
        assert!(first.score < 0.9, "score should have decayed on load");

        // Read the raw DB row to confirm the persisted value changed.
        let (db_score, db_ts): (f64, i64) = zeph_db::query_as(sql!(
            "SELECT score, updated_at_secs FROM mcp_trust_scores WHERE server_id = ?"
        ))
        .bind("srv1")
        .fetch_one(&pool)
        .await
        .unwrap();

        assert!(
            (db_score - first.score).abs() < 1e-9,
            "DB score must equal the decayed value after load(): db={db_score}, expected={}",
            first.score
        );
        assert!(
            db_ts > i64::try_from(old_ts).unwrap_or(0),
            "updated_at_secs must be refreshed after decay persist"
        );

        // Second immediate load must not decay further (timestamp was updated).
        let second = store.load("srv1").await.unwrap().unwrap();
        assert!(
            (second.score - first.score).abs() < 1e-6,
            "consecutive load() must not compound decay: first={}, second={}",
            first.score,
            second.score
        );
    }

    #[tokio::test]
    async fn store_load_no_write_when_no_decay() {
        let pool = test_pool().await;
        let store = TrustScoreStore::new(pool.clone());
        store.init().await.unwrap();

        // Insert a score at or below INITIAL_SCORE — no decay should trigger.
        let now_ts = unix_now();
        zeph_db::query(
            sql!("INSERT INTO mcp_trust_scores (server_id, score, success_count, failure_count, updated_at_secs)
             VALUES (?, ?, 0, 0, ?)"),
        )
        .bind("srv1")
        .bind(ServerTrustScore::INITIAL_SCORE)
        .bind(i64::try_from(now_ts).unwrap_or(i64::MAX))
        .execute(&pool)
        .await
        .unwrap();

        let loaded = store.load("srv1").await.unwrap().unwrap();
        assert!(
            (loaded.score - ServerTrustScore::INITIAL_SCORE).abs() < f64::EPSILON,
            "score at initial value should not decay"
        );

        // updated_at_secs in DB should remain approximately the same (no write occurred).
        let (db_ts,): (i64,) = zeph_db::query_as(sql!(
            "SELECT updated_at_secs FROM mcp_trust_scores WHERE server_id = ?"
        ))
        .bind("srv1")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            db_ts,
            i64::try_from(now_ts).unwrap_or(i64::MAX),
            "updated_at_secs must not change when no decay applied"
        );
    }

    #[tokio::test]
    async fn store_load_then_load_and_apply_delta_operates_on_decayed() {
        let pool = test_pool().await;
        let store = TrustScoreStore::new(pool.clone());
        store.init().await.unwrap();

        // Insert score=0.8 with timestamp 10 days ago.
        let old_ts = unix_now().saturating_sub(10 * 86_400);
        zeph_db::query(
            sql!("INSERT INTO mcp_trust_scores (server_id, score, success_count, failure_count, updated_at_secs)
             VALUES (?, ?, 0, 0, ?)"),
        )
        .bind("srv1")
        .bind(0.8_f64)
        .bind(i64::try_from(old_ts).unwrap_or(i64::MAX))
        .execute(&pool)
        .await
        .unwrap();

        // Trigger decay persistence via load(), which also refreshes updated_at_secs to now.
        let decayed = store.load("srv1").await.unwrap().unwrap();
        assert!(decayed.score < 0.8, "score must have decayed");

        // load_and_apply_delta() now sees a fresh updated_at_secs from the load() above, so
        // it must add the delta to the already-decayed score without re-decaying it again.
        store
            .load_and_apply_delta("srv1", ServerTrustScore::SUCCESS_BOOST, 1, 0)
            .await
            .unwrap();

        let final_score = store.load("srv1").await.unwrap().unwrap();
        let expected = (decayed.score + ServerTrustScore::SUCCESS_BOOST).min(1.0);
        assert!(
            (final_score.score - expected).abs() < 1e-6,
            "delta must be applied to decayed score without double-decaying: expected={expected}, got={}",
            final_score.score
        );
    }

    /// Regression for #6073: `load_and_apply_delta` must be atomic — two concurrent callers
    /// updating the same `server_id` must not lose either update. The former implementation
    /// did `load()` then an unconditional `UPDATE SET score = excluded.score`; if both callers
    /// read the same pre-update base score before either wrote back, the second write would
    /// clobber the first caller's delta. This is now impossible because the whole
    /// read-decay-delta-clamp-write sequence happens inside one `INSERT ... ON CONFLICT DO
    /// UPDATE` statement.
    const CONCURRENT_WRITERS: usize = 10;

    #[tokio::test]
    async fn load_and_apply_delta_concurrent_writers_no_lost_update() {
        let pool = test_pool().await;
        let store = Arc::new(TrustScoreStore::new(pool));
        store.init().await.unwrap();

        let mut set = tokio::task::JoinSet::new();
        for _ in 0..CONCURRENT_WRITERS {
            let store = Arc::clone(&store);
            set.spawn(async move {
                store
                    .load_and_apply_delta("srv1", ServerTrustScore::SUCCESS_BOOST, 1, 0)
                    .await
            });
        }
        while let Some(res) = set.join_next().await {
            res.expect("task panicked").expect("write failed");
        }

        let loaded = store.load("srv1").await.unwrap().unwrap();
        assert_eq!(
            loaded.success_count,
            u64::try_from(CONCURRENT_WRITERS).unwrap(),
            "every concurrent writer's success_count increment must be recorded"
        );
        // Deltas run near-instantly (no meaningful elapsed time), so decay is negligible —
        // the final score must reflect the sum of all deltas, clamped at 1.0.
        let writers = f64::from(u32::try_from(CONCURRENT_WRITERS).unwrap());
        let expected =
            (ServerTrustScore::INITIAL_SCORE + ServerTrustScore::SUCCESS_BOOST * writers).min(1.0);
        assert!(
            (loaded.score - expected).abs() < 1e-6,
            "lost update: expected score {expected} after {CONCURRENT_WRITERS} concurrent deltas, got {}",
            loaded.score
        );
    }

    #[tokio::test]
    async fn load_and_apply_delta_new_entry() {
        let pool = test_pool().await;
        let store = TrustScoreStore::new(pool);
        store.init().await.unwrap();

        store
            .load_and_apply_delta("srv1", ServerTrustScore::SUCCESS_BOOST, 1, 0)
            .await
            .unwrap();

        let loaded = store.load("srv1").await.unwrap().unwrap();
        assert!(
            loaded.score > ServerTrustScore::INITIAL_SCORE,
            "new entry should start at INITIAL_SCORE + delta"
        );
        assert_eq!(loaded.success_count, 1);
    }

    #[tokio::test]
    async fn load_and_apply_delta_applies_decay_before_delta() {
        let pool = test_pool().await;
        let store = TrustScoreStore::new(pool);
        store.init().await.unwrap();

        // Insert a high score with an old timestamp (simulate 30 days ago).
        let old_ts = unix_now().saturating_sub(30 * 86_400);
        zeph_db::query(
            sql!("INSERT INTO mcp_trust_scores (server_id, score, success_count, failure_count, updated_at_secs)
             VALUES (?, 0.9, 0, 0, ?)"),
        )
        .bind("srv1")
        .bind(i64::try_from(old_ts).unwrap())
        .execute(&store.pool)
        .await
        .unwrap();

        // Delta = 0.0 — decay only.
        store.load_and_apply_delta("srv1", 0.0, 0, 0).await.unwrap();

        let loaded = store.load("srv1").await.unwrap().unwrap();
        // After 30 days of decay (0.01/day) from 0.9, effective base ≈ 0.60.
        // Written back score should be below 0.9.
        assert!(
            loaded.score < 0.9,
            "score should have decayed from 0.9, got {}",
            loaded.score
        );
        assert!(
            loaded.score >= ServerTrustScore::INITIAL_SCORE,
            "score should not decay below INITIAL_SCORE, got {}",
            loaded.score
        );
    }

    /// Regression for #6073 (critic follow-up): the sibling test above only asserts loose
    /// bounds (`< 0.9`, `>= INITIAL_SCORE`) on the decayed score, which would not catch a
    /// subtly wrong decay-rate constant, an off-by-one in the elapsed-time expression, or a
    /// wrong divisor in the in-SQL `CASE WHEN` / `CAST(... AS REAL)` expression — any of
    /// those could still land within those loose bounds. This test pins the *exact* decayed
    /// value the atomic SQL statement must produce, computed independently in Rust from
    /// `ServerTrustScore::DECAY_RATE` and the row's actual persisted `updated_at_secs`
    /// (read back directly, bypassing `load()`, so this observes exactly what the atomic
    /// `INSERT ... ON CONFLICT DO UPDATE` wrote — not `load()`'s own separate decay pass).
    /// Runs unconditionally (not `#[ignore]`d) against the default in-memory `SQLite` pool.
    #[tokio::test]
    async fn load_and_apply_delta_decay_magnitude_is_exact() {
        let pool = test_pool().await;
        let store = TrustScoreStore::new(pool.clone());
        store.init().await.unwrap();

        // Insert a score above INITIAL_SCORE with a timestamp exactly 5 days in the past.
        let old_ts = i64::try_from(unix_now().saturating_sub(5 * 86_400)).unwrap();
        zeph_db::query(
            sql!("INSERT INTO mcp_trust_scores (server_id, score, success_count, failure_count, updated_at_secs)
             VALUES (?, 0.8, 0, 0, ?)"),
        )
        .bind("srv1")
        .bind(old_ts)
        .execute(&pool)
        .await
        .unwrap();

        // Delta = 0.0 isolates the in-SQL decay computation from delta application
        // (`excluded.score - INITIAL_SCORE` recovers to exactly 0.0).
        store.load_and_apply_delta("srv1", 0.0, 0, 0).await.unwrap();

        // Read the raw row back directly — NOT via `load()`, which would apply its own
        // separate Rust-side decay pass on top and defeat the purpose of this test.
        let (db_score, db_ts): (f64, i64) = zeph_db::query_as(sql!(
            "SELECT score, updated_at_secs FROM mcp_trust_scores WHERE server_id = ?"
        ))
        .bind("srv1")
        .fetch_one(&pool)
        .await
        .unwrap();

        // Replicate the SQL expression bit-for-bit using the row's *actual* persisted
        // `updated_at_secs` (the exact `now` the atomic statement bound), not a
        // separately-sampled clock reading — eliminates any timing-window flakiness.
        // Elapsed seconds fit comfortably in i32 (test uses a 5-day-old row) — narrow
        // before widening to f64 to keep the conversion exact and lint-clean.
        let elapsed_secs = i32::try_from(db_ts - old_ts).unwrap();
        let elapsed_days = f64::from(elapsed_secs) / 86_400.0;
        let expected = (0.8_f64 - ServerTrustScore::DECAY_RATE * elapsed_days)
            .max(ServerTrustScore::INITIAL_SCORE);

        assert!(
            (db_score - expected).abs() < 1e-9,
            "in-SQL decay magnitude mismatch: expected {expected}, got {db_score} \
             (elapsed_days={elapsed_days}, decay_rate={})",
            ServerTrustScore::DECAY_RATE
        );
        // Sanity: pin the test to actually exercising the mid-decay branch (not the
        // INITIAL_SCORE floor or a no-op), so a decay formula that always returns the
        // floor or always returns the input unchanged would still fail this test.
        assert!(
            db_score < 0.8 && db_score > ServerTrustScore::INITIAL_SCORE,
            "expected partial decay strictly between INITIAL_SCORE and 0.8, got {db_score}"
        );
    }
}
