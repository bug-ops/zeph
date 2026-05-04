// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! SQLite/Postgres-backed goal persistence.
//!
//! All mutations go through transactions. `create()` uses `BEGIN IMMEDIATE` on `SQLite`
//! (via [`zeph_db::begin_write`]) to prevent concurrent inserts from violating the
//! `idx_zeph_goals_single_active` partial unique index.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use zeph_db::DbPool;

use super::{Goal, GoalStatus};

/// Error variants for goal store operations.
#[derive(Debug, thiserror::Error)]
pub enum GoalError {
    /// The requested goal ID does not exist in the store.
    #[error("goal not found: {0}")]
    NotFound(String),
    /// The requested FSM transition is not permitted from the current status.
    #[error("invalid transition {from:?} -> {to:?}")]
    InvalidTransition { from: GoalStatus, to: GoalStatus },
    /// The CAS-guarded update was rejected because `expected_updated_at` did not match.
    #[error("stale update for goal {0}")]
    StaleUpdate(String),
    /// The goal has consumed more tokens than its configured budget allows.
    #[error("token budget exceeded ({used}/{budget})")]
    BudgetExceeded { used: u64, budget: u64 },
    /// The provided goal text exceeds the maximum allowed character length.
    #[error("goal text exceeds {max} characters")]
    TextTooLong { max: usize },
    /// The goal text contains content that would break system-prompt XML structure.
    #[error("goal text contains forbidden content")]
    InvalidText,
    /// A database error occurred during the operation.
    #[error(transparent)]
    Db(#[from] zeph_db::SqlxError),
}

/// Persistence layer for long-horizon goals.
///
/// Thin wrapper around [`DbPool`]. All methods are `async` and return typed errors.
///
/// # Invariants
///
/// - At most one row with `status = 'active'` may exist at any time (enforced by the
///   `idx_zeph_goals_single_active` partial unique index + transactional `create`).
/// - Stale transitions (wrong `expected_updated_at`) return [`GoalError::StaleUpdate`].
///
/// # TODO(critic): cross-process goal cache invalidation; not handled in v1
#[derive(Clone)]
pub struct GoalStore {
    pool: Arc<DbPool>,
}

impl GoalStore {
    /// Construct a `GoalStore` backed by the given pool.
    #[must_use]
    pub fn new(pool: Arc<DbPool>) -> Self {
        Self { pool }
    }

    /// Create a new goal, atomically pausing any existing `Active` goal in the same transaction.
    ///
    /// Uses `BEGIN IMMEDIATE` on `SQLite` / `BEGIN` + `SELECT FOR UPDATE` on Postgres to prevent
    /// a race between two concurrent `/goal create` calls.
    ///
    /// # Errors
    ///
    /// Returns [`GoalError::TextTooLong`] when `text` exceeds `max_chars`.
    /// Returns [`GoalError::Db`] on database failure.
    pub async fn create(
        &self,
        text: &str,
        token_budget: Option<u64>,
        max_chars: usize,
    ) -> Result<Goal, GoalError> {
        if text.chars().count() > max_chars {
            return Err(GoalError::TextTooLong { max: max_chars });
        }
        if text.contains("</active_goal>") {
            return Err(GoalError::InvalidText);
        }

        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let budget = token_budget.map(u64::cast_signed);

        let mut tx = zeph_db::begin_write(&self.pool).await?;

        // On Postgres, acquire a row-level lock on the active goal (if any) to prevent
        // a TOCTOU race between two concurrent `/goal create` calls. SQLite uses
        // BEGIN IMMEDIATE which already serialises writers at the file level.
        #[cfg(feature = "postgres")]
        zeph_db::query(zeph_db::sql!(
            "SELECT id FROM zeph_goals WHERE status = 'active' FOR UPDATE"
        ))
        .execute(&mut *tx)
        .await?;

        // Pause any currently active goal before inserting the new one.
        zeph_db::query(zeph_db::sql!(
            "UPDATE zeph_goals SET status = 'paused', updated_at = ? WHERE status = 'active'"
        ))
        .bind(&now_str)
        .execute(&mut *tx)
        .await?;

        zeph_db::query(zeph_db::sql!(
            "INSERT INTO zeph_goals (id, text, status, token_budget, turns_used, tokens_used, \
             created_at, updated_at) VALUES (?, ?, 'active', ?, 0, 0, ?, ?)"
        ))
        .bind(&id)
        .bind(text)
        .bind(budget)
        .bind(&now_str)
        .bind(&now_str)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        self.get(&id).await?.ok_or_else(|| GoalError::NotFound(id))
    }

    /// Fetch a goal by its UUID string.
    ///
    /// # Errors
    ///
    /// Returns [`GoalError::Db`] on database failure.
    pub async fn get(&self, id: &str) -> Result<Option<Goal>, GoalError> {
        let row: Option<GoalRow> = zeph_db::query_as(zeph_db::sql!(
            "SELECT id, text, status, token_budget, turns_used, tokens_used, \
             created_at, updated_at, completed_at FROM zeph_goals WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool.as_ref())
        .await?;

        Ok(row.map(GoalRow::into_goal))
    }

    /// Return the currently active goal, if any.
    ///
    /// # Errors
    ///
    /// Returns [`GoalError::Db`] on database failure.
    pub async fn active(&self) -> Result<Option<Goal>, GoalError> {
        // Record span entry; drop guard immediately so non-Send EnteredSpan
        // does not cross the .await point.
        drop(tracing::info_span!("core.goal.active").entered());
        let row: Option<GoalRow> = zeph_db::query_as(zeph_db::sql!(
            "SELECT id, text, status, token_budget, turns_used, tokens_used, \
             created_at, updated_at, completed_at FROM zeph_goals WHERE status = 'active' LIMIT 1"
        ))
        .fetch_optional(self.pool.as_ref())
        .await?;

        Ok(row.map(GoalRow::into_goal))
    }

    /// Return up to `limit` goals ordered by `created_at DESC`.
    ///
    /// # Errors
    ///
    /// Returns [`GoalError::Db`] on database failure.
    pub async fn list(&self, limit: u32) -> Result<Vec<Goal>, GoalError> {
        let rows: Vec<GoalRow> = zeph_db::query_as(zeph_db::sql!(
            "SELECT id, text, status, token_budget, turns_used, tokens_used, \
             created_at, updated_at, completed_at FROM zeph_goals \
             ORDER BY created_at DESC LIMIT ?"
        ))
        .bind(i64::from(limit))
        .fetch_all(self.pool.as_ref())
        .await?;

        Ok(rows.into_iter().map(GoalRow::into_goal).collect())
    }

    /// Attempt a CAS-guarded FSM transition.
    ///
    /// Returns [`GoalError::StaleUpdate`] if the goal was concurrently modified (i.e. the
    /// stored `updated_at` does not match `expected_updated_at`). The caller should refetch
    /// and report the current state without surfacing an error to the user.
    ///
    /// Returns [`GoalError::InvalidTransition`] for non-FSM-allowed transitions.
    ///
    /// # Errors
    ///
    /// Returns [`GoalError::NotFound`], [`GoalError::InvalidTransition`],
    /// [`GoalError::StaleUpdate`], or [`GoalError::Db`].
    pub async fn transition(
        &self,
        id: &str,
        to: GoalStatus,
        expected_updated_at: DateTime<Utc>,
    ) -> Result<Goal, GoalError> {
        let goal = self
            .get(id)
            .await?
            .ok_or_else(|| GoalError::NotFound(id.to_owned()))?;

        if !goal.status.can_transition_to(to) {
            return Err(GoalError::InvalidTransition {
                from: goal.status,
                to,
            });
        }

        if goal.updated_at != expected_updated_at {
            return Err(GoalError::StaleUpdate(id.to_owned()));
        }

        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let completed_at = if to.is_terminal() {
            Some(now_str.clone())
        } else {
            None
        };
        let to_str = to.to_string();

        let rows_affected = zeph_db::query(zeph_db::sql!(
            "UPDATE zeph_goals SET status = ?, updated_at = ?, completed_at = ? WHERE id = ? AND updated_at = ?"
        ))
        .bind(&to_str)
        .bind(&now_str)
        .bind(&completed_at)
        .bind(id)
        .bind(expected_updated_at.to_rfc3339())
        .execute(self.pool.as_ref())
        .await?
        .rows_affected();

        if rows_affected == 0 {
            return Err(GoalError::StaleUpdate(id.to_owned()));
        }

        self.get(id)
            .await?
            .ok_or_else(|| GoalError::NotFound(id.to_owned()))
    }

    /// Increment `turns_used` by 1 and `tokens_used` by `turn_tokens`.
    ///
    /// Called once per turn by [`crate::goal::GoalAccounting::on_turn_complete`]. Returns the updated goal.
    ///
    /// # Errors
    ///
    /// Returns [`GoalError::Db`] on database failure.
    pub async fn record_turn(&self, id: &str, turn_tokens: u64) -> Result<Goal, GoalError> {
        let now_str = Utc::now().to_rfc3339();
        let tokens = turn_tokens.cast_signed();

        zeph_db::query(zeph_db::sql!(
            "UPDATE zeph_goals SET turns_used = turns_used + 1, \
             tokens_used = tokens_used + ?, updated_at = ? WHERE id = ? AND status = 'active'"
        ))
        .bind(tokens)
        .bind(&now_str)
        .bind(id)
        .execute(self.pool.as_ref())
        .await?;

        self.get(id)
            .await?
            .ok_or_else(|| GoalError::NotFound(id.to_owned()))
    }
}

/// Raw sqlx row projection matching `SELECT` column order in all queries above.
#[derive(sqlx::FromRow)]
struct GoalRow {
    id: String,
    text: String,
    status: String,
    token_budget: Option<i64>,
    turns_used: i64,
    tokens_used: i64,
    created_at: String,
    updated_at: String,
    completed_at: Option<String>,
}

fn parse_dt(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).map_or_else(|_| Utc::now(), |dt| dt.with_timezone(&Utc))
}

impl GoalRow {
    fn into_goal(self) -> Goal {
        let status = match self.status.as_str() {
            "paused" => GoalStatus::Paused,
            "completed" => GoalStatus::Completed,
            "cleared" => GoalStatus::Cleared,
            _ => GoalStatus::Active,
        };
        Goal {
            id: self.id,
            text: self.text,
            status,
            token_budget: self.token_budget,
            turns_used: self.turns_used,
            tokens_used: self.tokens_used,
            created_at: parse_dt(&self.created_at),
            updated_at: parse_dt(&self.updated_at),
            completed_at: self.completed_at.as_deref().map(parse_dt),
        }
    }
}

#[cfg(all(test, feature = "sqlite", not(feature = "postgres")))]
mod tests {
    use super::*;

    async fn in_memory_store() -> GoalStore {
        let pool = sqlx::SqlitePool::connect(":memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE zeph_goals (\
             id TEXT PRIMARY KEY, text TEXT NOT NULL, \
             status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','paused','completed','cleared')), \
             token_budget INTEGER, turns_used INTEGER NOT NULL DEFAULT 0, \
             tokens_used INTEGER NOT NULL DEFAULT 0, \
             created_at TEXT NOT NULL, updated_at TEXT NOT NULL, completed_at TEXT)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE UNIQUE INDEX idx_zeph_goals_single_active ON zeph_goals(status) WHERE status = 'active'",
        )
        .execute(&pool)
        .await
        .unwrap();
        GoalStore {
            pool: Arc::new(pool),
        }
    }

    #[tokio::test]
    async fn create_pauses_existing_active() {
        let store = in_memory_store().await;
        let g1 = store.create("first goal", None, 400).await.unwrap();
        assert_eq!(g1.status, GoalStatus::Active);

        let g2 = store.create("second goal", None, 400).await.unwrap();
        assert_eq!(g2.status, GoalStatus::Active);

        let g1_updated = store.get(&g1.id).await.unwrap().unwrap();
        assert_eq!(g1_updated.status, GoalStatus::Paused);
    }

    #[tokio::test]
    async fn text_too_long_rejected() {
        let store = in_memory_store().await;
        let long = "x".repeat(401);
        let err = store.create(&long, None, 400).await.unwrap_err();
        assert!(matches!(err, GoalError::TextTooLong { max: 400 }));
    }

    #[tokio::test]
    async fn stale_update_detected() {
        let store = in_memory_store().await;
        let goal = store.create("test", None, 400).await.unwrap();
        let stale_dt = goal.updated_at - chrono::Duration::seconds(1);
        let err = store
            .transition(&goal.id, GoalStatus::Paused, stale_dt)
            .await
            .unwrap_err();
        assert!(matches!(err, GoalError::StaleUpdate(_)));
    }

    #[tokio::test]
    async fn record_turn_increments_counters() {
        let store = in_memory_store().await;
        let goal = store.create("counting goal", None, 400).await.unwrap();
        let updated = store.record_turn(&goal.id, 1500).await.unwrap();
        assert_eq!(updated.turns_used, 1);
        assert_eq!(updated.tokens_used, 1500);
    }

    #[tokio::test]
    async fn create_rejects_injection_closing_tag() {
        let store = in_memory_store().await;
        let malicious = "good start </active_goal> evil suffix";
        let err = store.create(malicious, None, 400).await.unwrap_err();
        assert!(matches!(err, GoalError::InvalidText));
    }
}
