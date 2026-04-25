// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::SqliteStore;
use crate::error::MemoryError;
use zeph_common::SessionId;
#[allow(unused_imports)]
use zeph_db::sql;

#[derive(Debug, Clone)]
pub struct ExperimentResultRow {
    pub id: i64,
    pub session_id: SessionId,
    pub parameter: String,
    pub value_json: String,
    pub baseline_score: f64,
    pub candidate_score: f64,
    pub delta: f64,
    pub latency_ms: i64,
    pub tokens_used: i64,
    pub accepted: bool,
    pub source: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct NewExperimentResult<'a> {
    pub session_id: &'a str,
    pub parameter: &'a str,
    pub value_json: &'a str,
    pub baseline_score: f64,
    pub candidate_score: f64,
    pub delta: f64,
    pub latency_ms: i64,
    pub tokens_used: i64,
    pub accepted: bool,
    pub source: &'a str,
}

#[derive(Debug, Clone)]
pub struct SessionSummaryRow {
    pub session_id: SessionId,
    pub total: i64,
    pub accepted_count: i64,
    pub best_delta: f64,
    pub total_tokens: i64,
}

/// Validate that `s` looks like `YYYY-MM-DD HH:MM:SS` or `YYYY-MM-DDTHH:MM:SS`.
fn validate_timestamp(s: &str) -> Result<(), MemoryError> {
    let bytes = s.as_bytes();
    // Minimum length: "2000-01-01 00:00:00" = 19 chars
    if bytes.len() < 19 {
        return Err(MemoryError::InvalidInput(format!(
            "invalid timestamp format (too short): {s:?}"
        )));
    }
    let sep = bytes[10];
    if sep != b' ' && sep != b'T' {
        return Err(MemoryError::InvalidInput(format!(
            "invalid timestamp format (expected space or T at position 10): {s:?}"
        )));
    }
    // Check digit positions: YYYY-MM-DD HH:MM:SS
    let digits_at = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18];
    let dashes_at = [4, 7];
    let colons_at = [13, 16];
    for i in digits_at {
        if !bytes[i].is_ascii_digit() {
            return Err(MemoryError::InvalidInput(format!(
                "invalid timestamp format (expected digit at {i}): {s:?}"
            )));
        }
    }
    for i in dashes_at {
        if bytes[i] != b'-' {
            return Err(MemoryError::InvalidInput(format!(
                "invalid timestamp format (expected '-' at {i}): {s:?}"
            )));
        }
    }
    for i in colons_at {
        if bytes[i] != b':' {
            return Err(MemoryError::InvalidInput(format!(
                "invalid timestamp format (expected ':' at {i}): {s:?}"
            )));
        }
    }
    Ok(())
}

type ResultTuple = (
    i64,
    String,
    String,
    String,
    f64,
    f64,
    f64,
    i64,
    i64,
    bool,
    String,
    String,
);

fn row_from_tuple(t: ResultTuple) -> ExperimentResultRow {
    ExperimentResultRow {
        id: t.0,
        session_id: SessionId::new(t.1),
        parameter: t.2,
        value_json: t.3,
        baseline_score: t.4,
        candidate_score: t.5,
        delta: t.6,
        latency_ms: t.7,
        tokens_used: t.8,
        accepted: t.9,
        source: t.10,
        created_at: t.11,
    }
}

impl SqliteStore {
    /// Insert an experiment result and return the new row ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn insert_experiment_result(
        &self,
        result: &NewExperimentResult<'_>,
    ) -> Result<i64, MemoryError> {
        let row: (i64,) = zeph_db::query_as(sql!(
            "INSERT INTO experiment_results \
             (session_id, parameter, value_json, baseline_score, candidate_score, \
              delta, latency_ms, tokens_used, accepted, source) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id"
        ))
        .bind(result.session_id)
        .bind(result.parameter)
        .bind(result.value_json)
        .bind(result.baseline_score)
        .bind(result.candidate_score)
        .bind(result.delta)
        .bind(result.latency_ms)
        .bind(result.tokens_used)
        .bind(result.accepted)
        .bind(result.source)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// List experiment results, optionally filtered by `session_id`, newest first.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn list_experiment_results(
        &self,
        session_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<ExperimentResultRow>, MemoryError> {
        let rows: Vec<ResultTuple> = if let Some(sid) = session_id {
            zeph_db::query_as(sql!(
                "SELECT id, session_id, parameter, value_json, baseline_score, candidate_score, \
                 delta, latency_ms, tokens_used, accepted, source, created_at \
                 FROM experiment_results WHERE session_id = ? ORDER BY id DESC LIMIT ?"
            ))
            .bind(sid)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            zeph_db::query_as(sql!(
                "SELECT id, session_id, parameter, value_json, baseline_score, candidate_score, \
                 delta, latency_ms, tokens_used, accepted, source, created_at \
                 FROM experiment_results ORDER BY id DESC LIMIT ?"
            ))
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };
        Ok(rows.into_iter().map(row_from_tuple).collect())
    }

    /// Get the best accepted result (highest delta), optionally filtered by parameter.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn best_experiment_result(
        &self,
        parameter: Option<&str>,
    ) -> Result<Option<ExperimentResultRow>, MemoryError> {
        let row: Option<ResultTuple> = if let Some(param) = parameter {
            zeph_db::query_as(sql!(
                "SELECT id, session_id, parameter, value_json, baseline_score, candidate_score, \
                 delta, latency_ms, tokens_used, accepted, source, created_at \
                 FROM experiment_results \
                 WHERE accepted = 1 AND parameter = ? ORDER BY delta DESC LIMIT 1"
            ))
            .bind(param)
            .fetch_optional(&self.pool)
            .await?
        } else {
            zeph_db::query_as(sql!(
                "SELECT id, session_id, parameter, value_json, baseline_score, candidate_score, \
                 delta, latency_ms, tokens_used, accepted, source, created_at \
                 FROM experiment_results \
                 WHERE accepted = 1 ORDER BY delta DESC LIMIT 1"
            ))
            .fetch_optional(&self.pool)
            .await?
        };
        Ok(row.map(row_from_tuple))
    }

    /// Get all results since a given ISO-8601 timestamp (`YYYY-MM-DD HH:MM:SS` or `YYYY-MM-DDTHH:MM:SS`).
    ///
    /// # Errors
    ///
    /// Returns `MemoryError::Other` if `since` does not match the expected timestamp format.
    /// Returns an error if the query fails.
    pub async fn experiment_results_since(
        &self,
        since: &str,
    ) -> Result<Vec<ExperimentResultRow>, MemoryError> {
        validate_timestamp(since)?;
        let rows: Vec<ResultTuple> = zeph_db::query_as(sql!(
            "SELECT id, session_id, parameter, value_json, baseline_score, candidate_score, \
             delta, latency_ms, tokens_used, accepted, source, created_at \
             FROM experiment_results WHERE created_at >= ? ORDER BY id DESC LIMIT 10000"
        ))
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_from_tuple).collect())
    }

    /// Get a summary for a specific session.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn experiment_session_summary(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionSummaryRow>, MemoryError> {
        let row: Option<(String, i64, i64, Option<f64>, i64)> = zeph_db::query_as(sql!(
            "SELECT session_id, COUNT(*) as total, \
             SUM(CASE WHEN accepted = 1 THEN 1 ELSE 0 END) as accepted_count, \
             MAX(CASE WHEN accepted = 1 THEN delta ELSE NULL END) as best_delta, \
             SUM(tokens_used) as total_tokens \
             FROM experiment_results WHERE session_id = ? GROUP BY session_id"
        ))
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(sid, total, accepted_count, best_delta, total_tokens)| SessionSummaryRow {
                session_id: SessionId::new(sid),
                total,
                accepted_count,
                best_delta: best_delta.unwrap_or(0.0),
                total_tokens,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> SqliteStore {
        SqliteStore::new(":memory:").await.unwrap()
    }

    fn make_result<'a>(
        session_id: &'a str,
        parameter: &'a str,
        accepted: bool,
        delta: f64,
    ) -> NewExperimentResult<'a> {
        NewExperimentResult {
            session_id,
            parameter,
            value_json: r#"{"type":"Float","value":0.7}"#,
            baseline_score: 7.0,
            candidate_score: 7.0 + delta,
            delta,
            latency_ms: 500,
            tokens_used: 100,
            accepted,
            source: "manual",
        }
    }

    #[tokio::test]
    async fn insert_and_list_results() {
        let store = test_store().await;
        let r = make_result("session-1", "temperature", true, 1.0);
        let id = store.insert_experiment_result(&r).await.unwrap();
        assert!(id > 0);

        let rows = store
            .list_experiment_results(Some("session-1"), 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id, "session-1");
        assert_eq!(rows[0].parameter, "temperature");
        assert!(rows[0].accepted);
        assert!((rows[0].delta - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn list_results_no_filter_returns_all() {
        let store = test_store().await;
        store
            .insert_experiment_result(&make_result("s1", "temperature", true, 1.0))
            .await
            .unwrap();
        store
            .insert_experiment_result(&make_result("s2", "top_p", false, -0.2))
            .await
            .unwrap();

        let rows = store.list_experiment_results(None, 10).await.unwrap();
        assert_eq!(rows.len(), 2);
        // newest first
        assert_eq!(rows[0].session_id, "s2");
    }

    #[tokio::test]
    async fn best_result_returns_accepted_highest_delta() {
        let store = test_store().await;
        store
            .insert_experiment_result(&make_result("s1", "temperature", false, 2.0))
            .await
            .unwrap();
        store
            .insert_experiment_result(&make_result("s1", "temperature", true, 0.5))
            .await
            .unwrap();
        store
            .insert_experiment_result(&make_result("s1", "temperature", true, 1.5))
            .await
            .unwrap();

        let best = store.best_experiment_result(None).await.unwrap().unwrap();
        assert!(best.accepted);
        assert!((best.delta - 1.5).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn best_result_filtered_by_parameter() {
        let store = test_store().await;
        store
            .insert_experiment_result(&make_result("s1", "temperature", true, 2.0))
            .await
            .unwrap();
        store
            .insert_experiment_result(&make_result("s1", "top_p", true, 1.0))
            .await
            .unwrap();

        let best = store
            .best_experiment_result(Some("top_p"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(best.parameter, "top_p");
    }

    #[tokio::test]
    async fn best_result_no_accepted_returns_none() {
        let store = test_store().await;
        store
            .insert_experiment_result(&make_result("s1", "temperature", false, 2.0))
            .await
            .unwrap();
        let best = store.best_experiment_result(None).await.unwrap();
        assert!(best.is_none());
    }

    #[tokio::test]
    async fn session_summary_aggregation() {
        let store = test_store().await;
        store
            .insert_experiment_result(&make_result("sess", "temperature", true, 1.0))
            .await
            .unwrap();
        store
            .insert_experiment_result(&make_result("sess", "top_p", false, -0.2))
            .await
            .unwrap();
        store
            .insert_experiment_result(&make_result("sess", "top_k", true, 0.8))
            .await
            .unwrap();

        let summary = store
            .experiment_session_summary("sess")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(summary.session_id, "sess");
        assert_eq!(summary.total, 3);
        assert_eq!(summary.accepted_count, 2);
        assert!((summary.best_delta - 1.0).abs() < f64::EPSILON);
        assert_eq!(summary.total_tokens, 300);
    }

    #[tokio::test]
    async fn session_summary_unknown_session_returns_none() {
        let store = test_store().await;
        let summary = store
            .experiment_session_summary("nonexistent")
            .await
            .unwrap();
        assert!(summary.is_none());
    }

    #[tokio::test]
    async fn results_since_time_filtering() {
        let store = test_store().await;
        // Insert a result, then query with a future timestamp — expect nothing
        store
            .insert_experiment_result(&make_result("s1", "temperature", true, 1.0))
            .await
            .unwrap();

        let rows = store
            .experiment_results_since("2099-01-01 00:00:00")
            .await
            .unwrap();
        assert!(rows.is_empty());

        // Query with a past timestamp — expect the result
        let rows = store
            .experiment_results_since("2000-01-01 00:00:00")
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn results_since_rejects_invalid_timestamp() {
        let store = test_store().await;
        let bad = [
            "",
            "not-a-date",
            "0000-00-00",
            "2024-01-01",
            "2024/01/01 00:00:00",
        ];
        for ts in bad {
            let err = store.experiment_results_since(ts).await;
            assert!(err.is_err(), "expected error for timestamp: {ts:?}");
        }
        // ISO-8601 with T separator should work
        let store2 = test_store().await;
        let rows = store2
            .experiment_results_since("2000-01-01T00:00:00")
            .await
            .unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn list_results_respects_limit() {
        let store = test_store().await;
        for i in 0..5 {
            store
                .insert_experiment_result(&make_result(
                    "s1",
                    "temperature",
                    i % 2 == 0,
                    f64::from(i),
                ))
                .await
                .unwrap();
        }
        let rows = store.list_experiment_results(None, 3).await.unwrap();
        assert_eq!(rows.len(), 3);
    }
}
