// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! PASTE — Pattern-Aware Speculative Tool Execution (issue #2409).
//!
//! Tracks per-skill tool invocation sequences in `SQLite` and surfaces top-K predicted
//! next tool calls at skill activation time. Predictions are scored using exponentially
//! decayed frequency × Wilson 95% one-sided lower bound on success rate.
//!
//! ## Scoring formula
//!
//! ```text
//! count_decayed = Σ_i  0.5 ^ ((now - t_i) / half_life_seconds)
//! p_hat         = success_raw / count_raw
//! wilson_low    = (p_hat + z²/(2n) - z·sqrt(p_hat(1-p_hat)/n + z²/(4n²))) / (1 + z²/n)
//!                 where n = count_raw, z = 1.645 (95% one-sided)
//! freq_norm     = count_decayed / total_decayed  (over sibling (skill_hash, prev_tool) rows)
//! score         = freq_norm * wilson_low
//! ```

#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, warn};
use zeph_db::DbPool;

use super::prediction::{Prediction, PredictionSource};
use crate::agent::speculative::cache::{args_template, hash_args};

/// Wilson 95% one-sided z-score.
const Z: f64 = 1.645;

/// Outcome of a tool call, used when observing a transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOutcome {
    Success,
    Failure,
}

/// Error type for `PatternStore` operations.
#[derive(Debug, Error)]
pub enum PatternError {
    #[error("database error: {0}")]
    Db(#[from] zeph_db::sqlx::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Per-`(skill_hash, prev_tool)` refresh debounce state.
struct RefreshState {
    last_refresh: Option<std::time::Instant>,
}

/// SQLite-backed tool invocation pattern store for PASTE.
///
/// Thread-safe via `Arc`. Call [`observe`](Self::observe) after each tool completion
/// and [`predict`](Self::predict) at skill activation to get speculative candidates.
///
/// # Examples
///
/// ```rust,no_run
/// # use zeph_core::agent::speculative::paste::{PatternStore, ToolOutcome};
/// # async fn example(pool: zeph_db::DbPool) -> Result<(), Box<dyn std::error::Error>> {
/// let store = PatternStore::new(pool, 14.0);
/// store.observe("my-skill", "abc123", None, "bash",
///     r#"{"command":"ls"}"#, ToolOutcome::Success, 42).await?;
/// let predictions = store.predict("my-skill", "abc123", None, 3).await?;
/// # Ok(())
/// # }
/// ```
pub struct PatternStore {
    pool: DbPool,
    half_life_days: f64,
    refresh_debounce: Arc<AsyncMutex<std::collections::HashMap<String, RefreshState>>>,
    min_observations: u32,
}

impl PatternStore {
    /// Create a new pattern store.
    ///
    /// `half_life_days` controls the exponential decay; 14 days is the default.
    #[must_use]
    pub fn new(pool: DbPool, half_life_days: f64) -> Self {
        Self {
            pool,
            half_life_days,
            refresh_debounce: Arc::new(AsyncMutex::new(std::collections::HashMap::new())),
            min_observations: 5,
        }
    }

    /// Set the minimum number of raw observations required before predicting.
    #[must_use]
    pub fn with_min_observations(mut self, n: u32) -> Self {
        self.min_observations = n;
        self
    }

    /// Record a completed tool invocation.
    ///
    /// Updates `count_decayed` using the exponential decay formula (computed in Rust,
    /// not via `pow()` in `SQLite` — `pow` requires `SQLITE_ENABLE_MATH_FUNCTIONS`
    /// which is not available in bundled `libsqlite3-sys`) and appends to
    /// `success_raw` / `count_raw`. Also triggers a debounced [`refresh`](Self::refresh).
    ///
    /// # Errors
    ///
    /// Returns [`PatternError::Db`] on `SQLite` failure.
    #[allow(clippy::too_many_arguments)]
    pub async fn observe(
        &self,
        skill_name: &str,
        skill_hash: &str,
        prev_tool: Option<&str>,
        next_tool: &str,
        args_json: &str,
        outcome: ToolOutcome,
        latency_ms: u64,
    ) -> Result<(), PatternError> {
        let now = unix_now();
        let half_life_secs = self.half_life_days * 86_400.0;
        let success_delta = i64::from(outcome == ToolOutcome::Success);
        let args: serde_json::Value = serde_json::from_str(args_json)?;
        let args_obj = args.as_object().cloned().unwrap_or_default();
        let args_fingerprint = {
            let h = hash_args(&args_obj);
            h.to_hex().to_string()
        };
        let tmpl = args_template(&args_obj);
        #[allow(clippy::cast_possible_wrap)]
        let latency_i64 = latency_ms as i64;

        // Fetch the existing row's count_decayed + last_seen_at so we can compute
        // the updated decay value in Rust (C6: avoids SQLite pow() which requires
        // SQLITE_ENABLE_MATH_FUNCTIONS not present in bundled libsqlite3-sys).
        let existing = zeph_db::query_as::<_, (f64, i64, i64, i64)>(
            r"
            SELECT count_decayed, last_seen_at, count_raw, avg_latency_ms
            FROM tool_pattern_transitions
            WHERE skill_name = ? AND skill_hash = ?
              AND (prev_tool = ? OR (prev_tool IS NULL AND ? IS NULL))
              AND next_tool = ? AND args_fingerprint = ?
            ",
        )
        .bind(skill_name)
        .bind(skill_hash)
        .bind(prev_tool)
        .bind(prev_tool)
        .bind(next_tool)
        .bind(&args_fingerprint)
        .fetch_optional(&self.pool)
        .await?;

        if let Some((old_decayed, last_seen_at, old_count_raw, old_avg_latency)) = existing {
            #[allow(clippy::cast_precision_loss)]
            let elapsed = (now - last_seen_at).max(0) as f64;
            let new_decayed = old_decayed * 0.5f64.powf(elapsed / half_life_secs) + 1.0;
            let new_count_raw = old_count_raw + 1;
            #[allow(clippy::cast_precision_loss)]
            let new_avg_latency = (old_avg_latency * old_count_raw + latency_i64) / new_count_raw;

            zeph_db::query(
                r"
                UPDATE tool_pattern_transitions SET
                    count_decayed  = ?,
                    count_raw      = ?,
                    success_raw    = success_raw + ?,
                    last_seen_at   = ?,
                    avg_latency_ms = ?
                WHERE skill_name = ? AND skill_hash = ?
                  AND (prev_tool = ? OR (prev_tool IS NULL AND ? IS NULL))
                  AND next_tool = ? AND args_fingerprint = ?
                ",
            )
            .bind(new_decayed)
            .bind(new_count_raw)
            .bind(success_delta)
            .bind(now)
            .bind(new_avg_latency)
            .bind(skill_name)
            .bind(skill_hash)
            .bind(prev_tool)
            .bind(prev_tool)
            .bind(next_tool)
            .bind(&args_fingerprint)
            .execute(&self.pool)
            .await?;
        } else {
            zeph_db::query(
                r"
                INSERT INTO tool_pattern_transitions
                    (skill_name, skill_hash, prev_tool, next_tool, args_fingerprint,
                     args_template, count_raw, success_raw, count_decayed, last_seen_at, avg_latency_ms)
                VALUES (?, ?, ?, ?, ?, ?, 1, ?, 1.0, ?, ?)
                ",
            )
            .bind(skill_name)
            .bind(skill_hash)
            .bind(prev_tool)
            .bind(next_tool)
            .bind(&args_fingerprint)
            .bind(&tmpl)
            .bind(success_delta)
            .bind(now)
            .bind(latency_i64)
            .execute(&self.pool)
            .await?;
        }

        self.debounced_refresh(skill_name, skill_hash, prev_tool)
            .await;
        Ok(())
    }

    /// Return the top-`k` predicted next tool calls for `(skill, prev_tool)`.
    ///
    /// Only returns predictions with `wilson_lower_bound >= 0.5` and
    /// `count_raw >= min_observations`.
    ///
    /// # Errors
    ///
    /// Returns [`PatternError::Db`] on `SQLite` failure.
    pub async fn predict(
        &self,
        skill_name: &str,
        skill_hash: &str,
        prev_tool: Option<&str>,
        k: u8,
    ) -> Result<Vec<Prediction>, PatternError> {
        let rows = zeph_db::query_as::<_, (String, String, f64, f64, i64)>(
            r"
            SELECT next_tool, args_template, score, wilson_lower_bound, rank
            FROM tool_pattern_predictions
            WHERE skill_name = ? AND skill_hash = ?
              AND (prev_tool = ? OR (prev_tool IS NULL AND ? IS NULL))
              AND wilson_lower_bound >= 0.5
            ORDER BY rank ASC
            LIMIT ?
            ",
        )
        .bind(skill_name)
        .bind(skill_hash)
        .bind(prev_tool)
        .bind(prev_tool)
        .bind(i64::from(k))
        .fetch_all(&self.pool)
        .await?;

        let predictions = rows
            .into_iter()
            .enumerate()
            .filter_map(|(i, (next_tool, args_template, score, _wilson, _rank))| {
                let args: serde_json::Map<String, serde_json::Value> =
                    serde_json::from_str(&args_template).ok()?;
                Some(Prediction {
                    tool_id: zeph_common::ToolName::new(next_tool),
                    args,
                    #[allow(clippy::cast_possible_truncation)]
                    confidence: score as f32,
                    source: PredictionSource::HistoryPattern {
                        skill: skill_name.to_owned(),
                        #[allow(clippy::cast_possible_truncation)]
                        rank: i as u8,
                    },
                })
            })
            .collect();

        Ok(predictions)
    }

    /// Recompute and materialize predictions for `(skill, skill_hash, prev_tool)`.
    ///
    /// Debounced to at most once per 60 s per `(skill_hash, prev_tool)`.
    /// Runs DELETE + N INSERTs inside a single transaction (H3: atomic refresh).
    ///
    /// # Errors
    ///
    /// Returns [`PatternError::Db`] on `SQLite` failure.
    pub async fn refresh(
        &self,
        skill_name: &str,
        skill_hash: &str,
        prev_tool: Option<&str>,
    ) -> Result<(), PatternError> {
        let min_obs = self.min_observations;
        let half_life_secs = self.half_life_days * 86_400.0;
        let now = unix_now();

        // Fetch all sibling transitions for Wilson + normalization.
        // Also fetch args_template so predictions carry the real type-placeholder template (H1).
        let rows = zeph_db::query_as::<_, (String, String, String, i64, i64, f64, i64)>(
            r"
            SELECT next_tool, args_fingerprint, args_template,
                   count_raw, success_raw, count_decayed, last_seen_at
            FROM tool_pattern_transitions
            WHERE skill_name = ? AND skill_hash = ?
              AND (prev_tool = ? OR (prev_tool IS NULL AND ? IS NULL))
              AND count_raw >= ?
            ",
        )
        .bind(skill_name)
        .bind(skill_hash)
        .bind(prev_tool)
        .bind(prev_tool)
        .bind(i64::from(min_obs))
        .fetch_all(&self.pool)
        .await?;

        if rows.is_empty() {
            return Ok(());
        }

        // Recompute decayed counts and Wilson scores in Rust, then normalize and rank.
        let scored = score_rows(rows, now, half_life_secs);
        if scored.is_empty() {
            return Ok(());
        }

        // Wrap DELETE + N INSERTs in a transaction to prevent partial state on crash (H3).
        let mut tx = zeph_db::begin(&self.pool).await?;

        zeph_db::query(
            "DELETE FROM tool_pattern_predictions \
             WHERE skill_name = ? AND skill_hash = ? \
             AND (prev_tool = ? OR (prev_tool IS NULL AND ? IS NULL))",
        )
        .bind(skill_name)
        .bind(skill_hash)
        .bind(prev_tool)
        .bind(prev_tool)
        .execute(&mut *tx)
        .await?;

        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        for (rank, (next_tool, args_fp, tmpl, score, wilson)) in scored.iter().enumerate().take(10)
        {
            let rank_i64 = rank as i64;
            zeph_db::query(
                r"
                INSERT OR REPLACE INTO tool_pattern_predictions
                    (skill_name, skill_hash, prev_tool, next_tool, args_fingerprint,
                     args_template, score, wilson_lower_bound, rank)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                ",
            )
            .bind(skill_name)
            .bind(skill_hash)
            .bind(prev_tool)
            .bind(next_tool)
            .bind(args_fp)
            .bind(tmpl)
            .bind(score)
            .bind(wilson)
            .bind(rank_i64)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;

        debug!(
            skill = skill_name,
            prev_tool = prev_tool.unwrap_or("<activation>"),
            "PASTE: refreshed {} predictions",
            scored.len().min(10)
        );
        Ok(())
    }

    /// Purge `tool_pattern_transitions` rows with stale `skill_hash` older than 30 days.
    ///
    /// # Errors
    ///
    /// Returns [`PatternError::Db`] on `SQLite` failure.
    pub async fn vacuum(&self) -> Result<u64, PatternError> {
        let cutoff = unix_now() - 30 * 86_400;
        let result = zeph_db::query("DELETE FROM tool_pattern_transitions WHERE last_seen_at < ?")
            .bind(cutoff)
            .execute(&self.pool)
            .await?;
        let rows = result.rows_affected();
        if rows > 0 {
            debug!("PASTE vacuum: removed {} stale rows", rows);
        }
        Ok(rows)
    }

    async fn debounced_refresh(&self, skill_name: &str, skill_hash: &str, prev_tool: Option<&str>) {
        let key = format!("{skill_hash}:{}", prev_tool.unwrap_or(""));
        let should_refresh = {
            let mut map = self.refresh_debounce.lock().await;
            let state = map
                .entry(key.clone())
                .or_insert(RefreshState { last_refresh: None });
            match state.last_refresh {
                None => true,
                Some(t) => t.elapsed() >= Duration::from_mins(1),
            }
        };
        if should_refresh {
            if let Err(e) = self.refresh(skill_name, skill_hash, prev_tool).await {
                warn!("PASTE refresh failed: {e}");
            }
            let mut map = self.refresh_debounce.lock().await;
            if let Some(state) = map.get_mut(&key) {
                state.last_refresh = Some(std::time::Instant::now());
            }
        }
    }
}

/// Compute decay-adjusted Wilson scores for a batch of transition rows and return them
/// sorted descending by score (top-K ready).
///
/// `rows` tuples: `(next_tool, args_fp, args_template, count_raw, success_raw, count_decayed, last_seen_at)`
fn score_rows(
    rows: Vec<(String, String, String, i64, i64, f64, i64)>,
    now: i64,
    half_life_secs: f64,
) -> Vec<(String, String, String, f64, f64)> {
    let decayed: Vec<_> = rows
        .into_iter()
        .map(
            |(tool, fp, tmpl, count_raw, success_raw, count_decayed, last_seen_at)| {
                #[allow(clippy::cast_precision_loss)]
                let elapsed = now.saturating_sub(last_seen_at) as f64;
                let current_decay = count_decayed * 0.5f64.powf(elapsed / half_life_secs);
                #[allow(clippy::cast_sign_loss)]
                let wilson = wilson_lower_bound(success_raw as u64, count_raw as u64);
                (tool, fp, tmpl, current_decay, wilson)
            },
        )
        .collect();

    let total: f64 = decayed.iter().map(|(_, _, _, d, _)| d).sum();
    if total <= 0.0 {
        return vec![];
    }

    let mut scored: Vec<_> = decayed
        .into_iter()
        .map(|(tool, fp, tmpl, d, wilson)| ((d / total) * wilson, tool, fp, tmpl, wilson))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored
        .into_iter()
        .map(|(score, tool, fp, tmpl, wilson)| (tool, fp, tmpl, score, wilson))
        .collect()
}

#[allow(clippy::cast_possible_wrap)]
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Wilson 95% one-sided lower confidence bound.
#[allow(clippy::cast_precision_loss)]
fn wilson_lower_bound(successes: u64, n: u64) -> f64 {
    if n == 0 {
        return 0.0;
    }
    let n = n as f64;
    let p_hat = successes as f64 / n;
    let z2 = Z * Z;
    let numerator =
        p_hat + z2 / (2.0 * n) - Z * (p_hat * (1.0 - p_hat) / n + z2 / (4.0 * n * n)).sqrt();
    let denominator = 1.0 + z2 / n;
    (numerator / denominator).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wilson_zero_observations() {
        assert_eq!(wilson_lower_bound(0, 0), 0.0);
    }

    #[test]
    fn wilson_all_success_small_n() {
        // 3/3 successes → lower bound well below 1.0 (small-sample conservatism)
        let lb = wilson_lower_bound(3, 3);
        assert!(lb > 0.0 && lb < 1.0, "got {lb}");
    }

    #[test]
    fn wilson_zero_success() {
        let lb = wilson_lower_bound(0, 10);
        assert!(lb < 0.1, "got {lb}");
    }

    #[test]
    fn fingerprint_deterministic_different_order() {
        fn fp(json: &str) -> String {
            let v: serde_json::Value = serde_json::from_str(json).unwrap();
            let obj = v.as_object().cloned().unwrap_or_default();
            hash_args(&obj).to_hex().to_string()
        }
        let a = r#"{"z": 1, "a": 2}"#;
        let b = r#"{"a": 2, "z": 1}"#;
        assert_eq!(fp(a), fp(b));
    }
}
