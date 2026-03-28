// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::time::{Duration, SystemTime, UNIX_EPOCH};
#[allow(unused_imports)]
use zeph_db::sql;

use serde::{Deserialize, Serialize};
use zeph_db::DbPool;

use crate::manager::McpTrustLevel;

/// Persistent per-server trust score with asymmetric time decay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerTrustScore {
    pub server_id: String,
    /// Cumulative score in `[0.0, 1.0]`. 0.5 = neutral (initial value).
    pub score: f64,
    pub success_count: u64,
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

    pub fn record_success(&mut self) {
        self.score = (self.score + Self::SUCCESS_BOOST).min(1.0);
        self.success_count += 1;
        self.updated_at_secs = unix_now();
    }

    pub fn record_failure(&mut self) {
        self.score = (self.score - Self::FAILURE_PENALTY).max(0.0);
        self.failure_count += 1;
        self.updated_at_secs = unix_now();
    }

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
    pub async fn init(&self) -> Result<(), zeph_db::DbError> {
        zeph_db::run_migrations(&self.pool).await?;
        Ok(())
    }

    /// Load the trust score for a server, applying asymmetric decay at read time.
    ///
    /// Decay is applied and, when non-zero, written back to the database so that
    /// subsequent `apply_delta()` calls operate on the true current (decayed) value
    /// rather than the stale stored score. Without this write-back, a success delta
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
    pub async fn load(&self, server_id: &str) -> Result<Option<ServerTrustScore>, sqlx::Error> {
        let row: Option<(String, f64, i64, i64, i64)> = sqlx::query_as(sql!(
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
            sqlx::query(sql!(
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

    /// Atomically apply a score delta and update counters.
    ///
    /// Uses `INSERT ... ON CONFLICT DO UPDATE SET score = score + ?` to prevent
    /// lost-update races from concurrent tool completions for the same server.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL execution fails.
    pub async fn apply_delta(
        &self,
        server_id: &str,
        score_delta: f64,
        success_increment: u64,
        failure_increment: u64,
    ) -> Result<(), sqlx::Error> {
        let now = i64::try_from(unix_now()).unwrap_or(i64::MAX);
        sqlx::query(sql!(
            "INSERT INTO mcp_trust_scores
                (server_id, score, success_count, failure_count, updated_at_secs)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(server_id) DO UPDATE SET
                score          = MIN(1.0, MAX(0.0, score + excluded.score - 0.5)),
                success_count  = success_count + excluded.success_count,
                failure_count  = failure_count + excluded.failure_count,
                updated_at_secs = excluded.updated_at_secs"
        ))
        .bind(server_id)
        // Initial insert: 0.5 + delta
        .bind(ServerTrustScore::INITIAL_SCORE + score_delta)
        .bind(i64::try_from(success_increment).unwrap_or(i64::MAX))
        .bind(i64::try_from(failure_increment).unwrap_or(i64::MAX))
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load the current score with decay applied, then write back the decayed-plus-delta value.
    ///
    /// Unlike [`apply_delta`], this method reads the stored score first, applies time-based
    /// decay in-memory, and then upserts the corrected value. This prevents delta application
    /// on a stale (pre-decay) score when a server has not been probed for an extended period.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL query or execution fails.
    pub async fn load_and_apply_delta(
        &self,
        server_id: &str,
        score_delta: f64,
        success_increment: u64,
        failure_increment: u64,
    ) -> Result<(), sqlx::Error> {
        let current = self.load(server_id).await?;
        let base_score = current.map_or(ServerTrustScore::INITIAL_SCORE, |s| s.score);
        let new_score = (base_score + score_delta).clamp(0.0, 1.0);
        let now = i64::try_from(unix_now()).unwrap_or(i64::MAX);
        sqlx::query(sql!(
            "INSERT INTO mcp_trust_scores
                (server_id, score, success_count, failure_count, updated_at_secs)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(server_id) DO UPDATE SET
                score           = excluded.score,
                success_count   = success_count + excluded.success_count,
                failure_count   = failure_count + excluded.failure_count,
                updated_at_secs = excluded.updated_at_secs"
        ))
        .bind(server_id)
        .bind(new_score)
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
    /// persists the decayed score so `apply_delta()` operates on the correct base value.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL query fails.
    pub async fn load_all(&self) -> Result<Vec<ServerTrustScore>, sqlx::Error> {
        let rows: Vec<(String, f64, i64, i64, i64)> = sqlx::query_as(sql!(
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
        store.apply_delta("srv1", 0.02, 1, 0).await.unwrap();

        let score = store.load("srv1").await.unwrap().unwrap();
        assert_eq!(score.server_id, "srv1");
        assert!(score.score > ServerTrustScore::INITIAL_SCORE);
        assert_eq!(score.success_count, 1);
        assert_eq!(score.failure_count, 0);
    }

    #[tokio::test]
    async fn store_apply_delta_failure() {
        let pool = test_pool().await;
        let store = TrustScoreStore::new(pool);
        store.init().await.unwrap();

        store
            .apply_delta("srv1", -ServerTrustScore::FAILURE_PENALTY, 0, 1)
            .await
            .unwrap();

        let score = store.load("srv1").await.unwrap().unwrap();
        assert!(score.score < ServerTrustScore::INITIAL_SCORE);
        assert_eq!(score.failure_count, 1);
    }

    #[tokio::test]
    async fn store_load_all_returns_all_servers() {
        let pool = test_pool().await;
        let store = TrustScoreStore::new(pool);
        store.init().await.unwrap();

        store.apply_delta("srv1", 0.0, 1, 0).await.unwrap();
        store.apply_delta("srv2", 0.0, 0, 1).await.unwrap();

        let all = store.load_all().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn store_atomic_update_does_not_reset() {
        let pool = test_pool().await;
        let store = TrustScoreStore::new(pool);
        store.init().await.unwrap();

        // Two consecutive success deltas.
        store.apply_delta("srv1", 0.02, 1, 0).await.unwrap();
        store.apply_delta("srv1", 0.02, 1, 0).await.unwrap();

        let score = store.load("srv1").await.unwrap().unwrap();
        assert_eq!(score.success_count, 2);
    }

    #[tokio::test]
    async fn store_score_clamped_between_zero_and_one() {
        let pool = test_pool().await;
        let store = TrustScoreStore::new(pool);
        store.init().await.unwrap();

        // Many large positive deltas — score must not exceed 1.0
        for _ in 0..50 {
            store.apply_delta("srv1", 0.5, 1, 0).await.unwrap();
        }
        let high = store.load("srv1").await.unwrap().unwrap();
        assert!(
            high.score <= 1.0,
            "score must not exceed 1.0, got {}",
            high.score
        );

        // Many large negative deltas — score must not go below 0.0
        for _ in 0..50 {
            store.apply_delta("srv2", -0.5, 0, 1).await.unwrap();
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
        sqlx::query(
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
        let (db_score, db_ts): (f64, i64) = sqlx::query_as(sql!(
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
        sqlx::query(
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
        let (db_ts,): (i64,) = sqlx::query_as(sql!(
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
    async fn store_load_then_delta_operates_on_decayed() {
        let pool = test_pool().await;
        let store = TrustScoreStore::new(pool.clone());
        store.init().await.unwrap();

        // Insert score=0.8 with timestamp 10 days ago.
        let old_ts = unix_now().saturating_sub(10 * 86_400);
        sqlx::query(
            sql!("INSERT INTO mcp_trust_scores (server_id, score, success_count, failure_count, updated_at_secs)
             VALUES (?, ?, 0, 0, ?)"),
        )
        .bind("srv1")
        .bind(0.8_f64)
        .bind(i64::try_from(old_ts).unwrap_or(i64::MAX))
        .execute(&pool)
        .await
        .unwrap();

        // Trigger decay persistence via load().
        let decayed = store.load("srv1").await.unwrap().unwrap();
        assert!(decayed.score < 0.8, "score must have decayed");

        // apply_delta now operates on the persisted decayed score, not 0.8.
        store
            .apply_delta("srv1", ServerTrustScore::SUCCESS_BOOST, 1, 0)
            .await
            .unwrap();

        let final_score = store.load("srv1").await.unwrap().unwrap();
        let expected = (decayed.score + ServerTrustScore::SUCCESS_BOOST).min(1.0);
        assert!(
            (final_score.score - expected).abs() < 1e-6,
            "delta must be applied to decayed score: expected={expected}, got={}",
            final_score.score
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

        let score = store.load("srv1").await.unwrap().unwrap();
        assert!(
            score.score > ServerTrustScore::INITIAL_SCORE,
            "new entry should start at INITIAL_SCORE + delta"
        );
        assert_eq!(score.success_count, 1);
    }

    #[tokio::test]
    async fn load_and_apply_delta_applies_decay_before_delta() {
        let pool = test_pool().await;
        let store = TrustScoreStore::new(pool);
        store.init().await.unwrap();

        // Insert a high score with an old timestamp (simulate 30 days ago).
        let old_ts = unix_now().saturating_sub(30 * 86_400);
        sqlx::query(
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

        let score = store.load("srv1").await.unwrap().unwrap();
        // After 30 days of decay (0.01/day) from 0.9, effective base ≈ 0.60.
        // Written back score should be below 0.9.
        assert!(
            score.score < 0.9,
            "score should have decayed from 0.9, got {}",
            score.score
        );
        assert!(
            score.score >= ServerTrustScore::INITIAL_SCORE,
            "score should not decay below INITIAL_SCORE, got {}",
            score.score
        );
    }
}
