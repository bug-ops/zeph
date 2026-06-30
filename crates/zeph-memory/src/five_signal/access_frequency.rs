// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;

use sqlx::Row as _;
use zeph_db::DbPool;

use crate::types::MessageId;

/// Cap applied before log-normalization to prevent a single ultra-hot fact from
/// dominating frequency scores across the candidate set.
const MAX_ACCESS_COUNT: f64 = 10_000.0;

/// Per-turn access frequency aggregator backed by `fact_access_log`.
///
/// Loads raw access counts for a candidate set in a single `GROUP BY` query per turn.
/// Normalized values are `log(1 + count) / log(1 + MAX_ACCESS_COUNT)` ∈ `[0.0, 1.0]`.
pub struct AccessFrequencyCache {
    pool: DbPool,
}

impl AccessFrequencyCache {
    /// Create a new cache backed by the given pool.
    #[must_use]
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Load and normalize access counts for `fact_ids` within `session_id`.
    ///
    /// Issues a single SQL `GROUP BY` query indexed by `(session_id, accessed_at DESC)`.
    /// Returns a map of `fact_id → normalized_score ∈ [0.0, 1.0]`.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[tracing::instrument(
        name = "memory.five_signal.access_frequency.load",
        skip(self, fact_ids),
        fields(fact_count = fact_ids.len())
    )]
    pub async fn load_for_candidates(
        &self,
        session_id: &str,
        fact_ids: &[MessageId],
    ) -> Result<HashMap<MessageId, f64>, crate::error::MemoryError> {
        tracing::debug!("five_signal: loading access frequencies");

        if fact_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let ids: Vec<i64> = fact_ids.iter().map(|id| id.0).collect();

        // sqlx does not support binding Vec<i64> with IN directly for all backends;
        // build the query with placeholders manually.
        let placeholders: String = ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "SELECT fact_id, COUNT(*) as cnt FROM fact_access_log \
             WHERE session_id = ?1 AND fact_id IN ({placeholders}) \
             GROUP BY fact_id"
        );

        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(session_id);
        for id in &ids {
            q = q.bind(id);
        }

        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| crate::error::MemoryError::Db(e.into()))?;

        let counts: HashMap<i64, i64> = rows
            .iter()
            .map(|row| (row.get::<i64, _>("fact_id"), row.get::<i64, _>("cnt")))
            .collect();

        let normalized = fact_ids
            .iter()
            .map(|id| {
                #[expect(clippy::cast_precision_loss)]
                let raw = *counts.get(&id.0).unwrap_or(&0) as f64;
                let score =
                    (1.0_f64 + raw.min(MAX_ACCESS_COUNT)).ln() / (1.0 + MAX_ACCESS_COUNT).ln();
                (*id, score)
            })
            .collect();

        Ok(normalized)
    }

    /// Record a fact access event in `fact_access_log`.
    ///
    /// Failures are logged as `WARN` and do not propagate — access logging is non-critical.
    #[tracing::instrument(
        name = "memory.five_signal.access_frequency.log",
        skip(self, fact_type, session_id),
        fields(fact_id = fact_id.0)
    )]
    pub async fn log_access(&self, fact_id: MessageId, fact_type: &str, session_id: &str) {
        tracing::debug!("five_signal: logging access");

        let accessed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));

        let res = sqlx::query(
            "INSERT INTO fact_access_log (fact_id, fact_type, session_id, accessed_at) \
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(fact_id.0)
        .bind(fact_type)
        .bind(session_id)
        .bind(accessed_at)
        .execute(&self.pool)
        .await;

        if let Err(e) = res {
            tracing::warn!(
                fact_id = fact_id.0,
                error = %e,
                "five_signal: failed to log fact access (non-fatal)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> DbPool {
        crate::store::SqliteStore::with_pool_size(":memory:", 1)
            .await
            .expect("in-memory SQLite failed")
            .pool()
            .clone()
    }

    #[tokio::test]
    async fn load_for_candidates_empty_returns_empty() {
        let pool = test_pool().await;
        let cache = AccessFrequencyCache::new(pool);
        let result = cache.load_for_candidates("s1", &[]).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn load_for_candidates_no_rows_gives_zero_score() {
        let pool = test_pool().await;
        let cache = AccessFrequencyCache::new(pool);
        let ids = vec![MessageId(1), MessageId(2)];
        let scores = cache.load_for_candidates("s1", &ids).await.unwrap();
        assert_eq!(scores.len(), 2);
        assert!(scores[&MessageId(1)] < f64::EPSILON);
        assert!(scores[&MessageId(2)] < f64::EPSILON);
    }

    #[tokio::test]
    async fn load_for_candidates_higher_count_gives_higher_score() {
        let pool = test_pool().await;
        let cache = AccessFrequencyCache::new(pool.clone());
        let session = "test-session";

        // Insert 1 access for fact 10 and 5 accesses for fact 20.
        sqlx::query(
            "INSERT INTO fact_access_log (fact_id, fact_type, session_id, accessed_at) \
             VALUES (?1, 'episodic', ?2, 0)",
        )
        .bind(10_i64)
        .bind(session)
        .execute(&pool)
        .await
        .unwrap();

        for _ in 0..5_u8 {
            sqlx::query(
                "INSERT INTO fact_access_log (fact_id, fact_type, session_id, accessed_at) \
                 VALUES (?1, 'episodic', ?2, 0)",
            )
            .bind(20_i64)
            .bind(session)
            .execute(&pool)
            .await
            .unwrap();
        }

        let ids = vec![MessageId(10), MessageId(20)];
        let scores = cache.load_for_candidates(session, &ids).await.unwrap();

        let s10 = scores[&MessageId(10)];
        let s20 = scores[&MessageId(20)];
        assert!(
            s20 > s10,
            "higher access count must yield higher score: {s20} vs {s10}"
        );
        assert!(s10 > 0.0, "score for fact with 1 access must be > 0");
        assert!(s20 <= 1.0, "score must be capped at 1.0");
    }

    #[tokio::test]
    async fn load_for_candidates_ignores_other_sessions() {
        let pool = test_pool().await;
        let cache = AccessFrequencyCache::new(pool.clone());

        sqlx::query(
            "INSERT INTO fact_access_log (fact_id, fact_type, session_id, accessed_at) \
             VALUES (?1, 'episodic', ?2, 0)",
        )
        .bind(99_i64)
        .bind("other-session")
        .execute(&pool)
        .await
        .unwrap();

        let ids = vec![MessageId(99)];
        let scores = cache.load_for_candidates("my-session", &ids).await.unwrap();
        assert!(
            scores[&MessageId(99)] < f64::EPSILON,
            "score must be 0 for different session"
        );
    }

    #[test]
    fn normalization_zero_count() {
        let raw = 0.0_f64;
        let score = (1.0 + raw.min(MAX_ACCESS_COUNT)).ln() / (1.0 + MAX_ACCESS_COUNT).ln();
        assert!((score).abs() < 1e-9, "zero access → score 0.0");
    }

    #[test]
    fn normalization_max_count() {
        let raw = MAX_ACCESS_COUNT;
        let score = (1.0 + raw.min(MAX_ACCESS_COUNT)).ln() / (1.0 + MAX_ACCESS_COUNT).ln();
        assert!((score - 1.0).abs() < 1e-9, "max access → score 1.0");
    }

    #[test]
    fn normalization_overflow_clamped() {
        let raw = MAX_ACCESS_COUNT * 2.0;
        let score = (1.0 + raw.min(MAX_ACCESS_COUNT)).ln() / (1.0 + MAX_ACCESS_COUNT).ln();
        assert!((score - 1.0).abs() < 1e-9, "overflow is clamped to 1.0");
    }

    #[test]
    fn normalization_monotone() {
        let score_low = (1.0 + 10.0_f64.min(MAX_ACCESS_COUNT)).ln() / (1.0 + MAX_ACCESS_COUNT).ln();
        let score_high =
            (1.0 + 100.0_f64.min(MAX_ACCESS_COUNT)).ln() / (1.0 + MAX_ACCESS_COUNT).ln();
        assert!(score_high > score_low);
    }
}
