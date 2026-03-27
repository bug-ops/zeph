// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

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
    pool: SqlitePool,
}

impl TrustScoreStore {
    /// Create a new store backed by the given pool.
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Initialize the `mcp_trust_scores` table.
    ///
    /// Uses `CREATE TABLE IF NOT EXISTS` for forward compatibility. New columns can be
    /// added via `ALTER TABLE ADD COLUMN` without breaking existing databases.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL execution fails.
    pub async fn init(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS mcp_trust_scores (
                server_id      TEXT PRIMARY KEY NOT NULL,
                score          REAL NOT NULL DEFAULT 0.5,
                success_count  INTEGER NOT NULL DEFAULT 0,
                failure_count  INTEGER NOT NULL DEFAULT 0,
                updated_at_secs INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load the trust score for a server, applying asymmetric decay at read time.
    ///
    /// Decay is applied in-memory on the returned value; it is NOT written back to the
    /// database here. Callers that need to persist the decayed score should call
    /// `apply_delta(server_id, 0.0, 0, 0)` after reading.
    ///
    /// Returns `None` if not found.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL query fails.
    pub async fn load(&self, server_id: &str) -> Result<Option<ServerTrustScore>, sqlx::Error> {
        let row: Option<(String, f64, i64, i64, i64)> = sqlx::query_as(
            "SELECT server_id, score, success_count, failure_count, updated_at_secs
             FROM mcp_trust_scores WHERE server_id = ?",
        )
        .bind(server_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|(sid, score, sc, fc, ts)| {
            let mut entry = ServerTrustScore {
                server_id: sid,
                score,
                success_count: u64::try_from(sc).unwrap_or(0),
                failure_count: u64::try_from(fc).unwrap_or(0),
                updated_at_secs: u64::try_from(ts).unwrap_or(0),
            };
            entry.apply_decay();
            entry
        }))
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
        sqlx::query(
            "INSERT INTO mcp_trust_scores
                (server_id, score, success_count, failure_count, updated_at_secs)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(server_id) DO UPDATE SET
                score          = MIN(1.0, MAX(0.0, score + excluded.score - 0.5)),
                success_count  = success_count + excluded.success_count,
                failure_count  = failure_count + excluded.failure_count,
                updated_at_secs = excluded.updated_at_secs",
        )
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

    /// Load all server trust scores.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL query fails.
    pub async fn load_all(&self) -> Result<Vec<ServerTrustScore>, sqlx::Error> {
        let rows: Vec<(String, f64, i64, i64, i64)> = sqlx::query_as(
            "SELECT server_id, score, success_count, failure_count, updated_at_secs
             FROM mcp_trust_scores",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(sid, score, sc, fc, ts)| ServerTrustScore {
                server_id: sid,
                score,
                success_count: u64::try_from(sc).unwrap_or(0),
                failure_count: u64::try_from(fc).unwrap_or(0),
                updated_at_secs: u64::try_from(ts).unwrap_or(0),
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
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
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
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
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
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let store = TrustScoreStore::new(pool);
        store.init().await.unwrap();

        store.apply_delta("srv1", 0.0, 1, 0).await.unwrap();
        store.apply_delta("srv2", 0.0, 0, 1).await.unwrap();

        let all = store.load_all().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn store_atomic_update_does_not_reset() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
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
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
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
    async fn store_load_before_init_returns_error() {
        // Without calling init(), the table does not exist — load() must return Err.
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let store = TrustScoreStore::new(pool);
        // Do NOT call store.init()
        let result = store.load("srv1").await;
        assert!(
            result.is_err(),
            "load before init should return a SQL error"
        );
    }
}
