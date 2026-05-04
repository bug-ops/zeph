// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-turn goal token accounting (G4).
//!
//! `GoalAccounting` is the bridge between the agent loop and `GoalStore`. It caches
//! the active goal ID in memory to avoid a round-trip on every turn when no goal is
//! active, and dispatches `record_turn` as a fire-and-forget background task tracked
//! by the agent supervisor.

use std::sync::Arc;

use parking_lot::Mutex;

use super::{Goal, GoalSnapshot, GoalStatus, GoalStore, store::GoalError};

/// Cached state for the current active goal.
struct CachedGoal {
    id: String,
    text: String,
    status: GoalStatus,
    token_budget: Option<u64>,
}

/// Per-turn token accounting service for the active long-horizon goal.
///
/// Wraps `GoalStore` with an in-memory cache of the active goal ID so that
/// turns without an active goal incur no database round-trips.
///
/// # Invariant (G4)
///
/// `on_turn_complete` is fire-and-forget. A DB write failure logs `WARN` and never
/// aborts the turn. Budget exhaustion auto-pauses the goal via a best-effort
/// background task.
pub struct GoalAccounting {
    store: Arc<GoalStore>,
    cached: Mutex<Option<CachedGoal>>,
}

impl GoalAccounting {
    /// Create a new `GoalAccounting` backed by `store`.
    #[must_use]
    pub fn new(store: Arc<GoalStore>) -> Self {
        Self {
            store,
            cached: Mutex::new(None),
        }
    }

    /// Refresh the in-memory cache from the database.
    ///
    /// Call this after every `/goal` command to ensure the cache reflects the
    /// latest state before the next turn.
    ///
    /// # Errors
    ///
    /// Returns [`GoalError`] if the database query fails.
    pub async fn refresh(&self) -> Result<(), GoalError> {
        let active = self.store.active().await?;
        let mut guard = self.cached.lock();
        *guard = active.map(|g| CachedGoal {
            id: g.id,
            text: g.text,
            status: g.status,
            token_budget: g.token_budget.map(|b| b.max(0).cast_unsigned()),
        });
        Ok(())
    }

    /// Build a lightweight snapshot of the cached active goal, if any.
    ///
    /// Returns `None` when no goal is active (the goal was cleared, completed,
    /// paused, or never created).
    pub fn snapshot(&self) -> Option<GoalSnapshot> {
        let guard = self.cached.lock();
        let cached = guard.as_ref()?;
        if cached.status != GoalStatus::Active {
            return None;
        }
        Some(GoalSnapshot {
            id: cached.id.clone(),
            text: cached.text.clone(),
            status: cached.status,
            turns_used: 0,
            tokens_used: 0,
            token_budget: cached.token_budget,
        })
    }

    /// Notify the accounting service that a turn completed, consuming `turn_tokens` tokens.
    ///
    /// If no active goal is cached, this is a cheap no-op (no DB access).
    ///
    /// When a token budget is set and exceeded, the goal is auto-paused via a
    /// best-effort background task.
    ///
    /// Background tasks are spawned via the provided closure so the caller controls
    /// how they are tracked (typically via the agent supervisor).
    pub fn on_turn_complete(
        &self,
        turn_tokens: u64,
        spawn_bg: impl FnOnce(std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>),
    ) {
        let goal_id = {
            let guard = self.cached.lock();
            let Some(cached) = guard.as_ref() else { return };
            if cached.status != GoalStatus::Active {
                return;
            }
            cached.id.clone()
        };

        let store = Arc::clone(&self.store);

        spawn_bg(Box::pin(async move {
            match store.record_turn(&goal_id, turn_tokens).await {
                Ok(updated) => {
                    tracing::debug!(
                        goal_id = %goal_id,
                        turns_used = updated.turns_used,
                        tokens_used = updated.tokens_used,
                        "goal accounting: turn recorded"
                    );
                    // Auto-pause when token budget exceeded.
                    if let (Some(budget), tokens_used) = (
                        updated.token_budget,
                        updated.tokens_used.max(0).cast_unsigned(),
                    ) {
                        let budget = budget.max(0).cast_unsigned();
                        if tokens_used >= budget {
                            tracing::warn!(
                                goal_id = %goal_id,
                                tokens_used,
                                budget,
                                "goal token budget exhausted — auto-pausing"
                            );
                            match store
                                .transition(&goal_id, GoalStatus::Paused, updated.updated_at)
                                .await
                            {
                                Ok(_) => {}
                                Err(GoalError::StaleUpdate(_)) => {
                                    tracing::warn!(
                                        goal_id = %goal_id,
                                        "goal auto-pause skipped: concurrent modification (stale update)"
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(goal_id = %goal_id, error = %e, "goal auto-pause failed");
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(goal_id = %goal_id, error = %e, "goal accounting: record_turn failed");
                }
            }
        }));
    }

    /// Return a full `Goal` from the store for a given id.
    ///
    /// Used by the `/goal status` handler to show live details.
    ///
    /// # Errors
    ///
    /// Returns [`GoalError`] if the database query fails.
    pub async fn get_active(&self) -> Result<Option<Goal>, GoalError> {
        self.store.active().await
    }

    /// Return a reference to the underlying store for direct queries.
    #[must_use]
    pub fn get_store(&self) -> Arc<GoalStore> {
        Arc::clone(&self.store)
    }
}

#[cfg(all(test, feature = "sqlite", not(feature = "postgres")))]
mod tests {
    use std::sync::Arc;

    use super::*;

    async fn make_store() -> Arc<GoalStore> {
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
        Arc::new(GoalStore::new(Arc::new(pool)))
    }

    #[tokio::test]
    async fn snapshot_returns_none_when_no_active_goal() {
        let store = make_store().await;
        let accounting = GoalAccounting::new(store);
        assert!(accounting.snapshot().is_none());
    }

    #[tokio::test]
    async fn refresh_populates_cache_from_db() {
        let store = make_store().await;
        store.create("buy groceries", None, 400).await.unwrap();

        let accounting = GoalAccounting::new(Arc::clone(&store));
        assert!(accounting.snapshot().is_none(), "cache starts empty");

        accounting.refresh().await.unwrap();
        let snap = accounting.snapshot().expect("snapshot after refresh");
        assert_eq!(snap.text, "buy groceries");
        assert_eq!(snap.status, GoalStatus::Active);
    }

    #[tokio::test]
    async fn snapshot_returns_none_for_paused_goal() {
        let store = make_store().await;
        let goal = store.create("do thing", None, 400).await.unwrap();
        store
            .transition(&goal.id, GoalStatus::Paused, goal.updated_at)
            .await
            .unwrap();

        let accounting = GoalAccounting::new(Arc::clone(&store));
        accounting.refresh().await.unwrap();
        // Paused goal should not appear in snapshot.
        assert!(accounting.snapshot().is_none());
    }

    #[tokio::test]
    async fn on_turn_complete_is_noop_when_cache_empty() {
        let store = make_store().await;
        let accounting = GoalAccounting::new(store);
        let mut called = false;
        accounting.on_turn_complete(100, |_fut| {
            called = true;
        });
        assert!(!called, "spawn_bg must not be called when no active goal");
    }

    #[tokio::test]
    async fn on_turn_complete_spawns_background_task() {
        let store = make_store().await;
        store.create("active goal", None, 400).await.unwrap();

        let accounting = GoalAccounting::new(Arc::clone(&store));
        accounting.refresh().await.unwrap();

        let mut fut_received: Option<
            std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        > = None;
        accounting.on_turn_complete(500, |fut| {
            fut_received = Some(fut);
        });
        assert!(
            fut_received.is_some(),
            "spawn_bg must be called when active goal exists"
        );

        // Drive the future to completion.
        fut_received.unwrap().await;

        // Verify tokens were recorded.
        let goals = store.list(10).await.unwrap();
        let active = goals
            .iter()
            .find(|g| g.status == GoalStatus::Active)
            .unwrap();
        assert_eq!(active.tokens_used, 500);
        assert_eq!(active.turns_used, 1);
    }

    #[tokio::test]
    async fn auto_pause_on_budget_exhaustion() {
        let store = make_store().await;
        // Budget of 100 tokens; turn consumes 200.
        store.create("budget goal", Some(100), 400).await.unwrap();

        let accounting = GoalAccounting::new(Arc::clone(&store));
        accounting.refresh().await.unwrap();

        let mut fut_received = None;
        accounting.on_turn_complete(200, |fut| {
            fut_received = Some(fut);
        });
        fut_received.unwrap().await;

        // Goal should now be paused.
        let goals = store.list(10).await.unwrap();
        let goal = goals.first().unwrap();
        assert_eq!(
            goal.status,
            GoalStatus::Paused,
            "goal must be auto-paused when budget exhausted"
        );
    }
}
