// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Advisory entity locking for multi-agent `GraphStore` coordination (#2478).
//!
//! `SQLite` does not provide row-level locks. This module implements a soft advisory
//! locking pattern using a dedicated `entity_advisory_locks` table. Locks are
//! automatically expired after 120 seconds (covers worst-case slow LLM calls).
//!
//! Expired locks are reclaimed on the next `try_acquire` call rather than via a
//! cleanup sweep. When a lock is reclaimed by another session, the original holder's
//! subsequent writes follow last-writer-wins semantics — acceptable for entity
//! resolution where duplicate entities can be merged in a later consolidation sweep.

use std::time::Duration;

use tokio::time::sleep;
use zeph_common::SessionId;
use zeph_db::{DbPool, query, query_scalar, sql};

use crate::error::MemoryError;

/// TTL for advisory locks in seconds. Must exceed the worst-case LLM call latency.
const LOCK_TTL_SECS: i64 = 120;

/// Maximum retry attempts when a lock is held by another session.
const MAX_RETRIES: u32 = 3;

/// Base backoff duration for lock acquisition retries.
const BASE_BACKOFF_MS: u64 = 50;

/// Advisory entity lock manager for a single session.
pub struct EntityLockManager {
    pool: DbPool,
    session_id: SessionId,
}

impl EntityLockManager {
    /// Create an `EntityLockManager` for the given session.
    ///
    /// Accepts anything convertible to [`SessionId`]: a `SessionId` directly,
    /// a `&str`, or a `String`.
    #[must_use]
    pub fn new(pool: DbPool, session_id: impl Into<SessionId>) -> Self {
        Self {
            pool,
            session_id: session_id.into(),
        }
    }

    /// Try to acquire an advisory lock on `entity_name`.
    ///
    /// - If no lock exists: INSERT and return `true`.
    /// - If the current session already holds the lock: UPDATE `expires_at`, return `true`.
    /// - If another session holds a non-expired lock: retry with exponential backoff.
    /// - After `MAX_RETRIES` failures: return `false` (caller proceeds without lock).
    ///
    /// Expired locks (past `expires_at`) are atomically reclaimed on the INSERT conflict.
    ///
    /// # Errors
    ///
    /// Returns an error on database failures.
    pub async fn try_acquire(&self, entity_name: &str) -> Result<bool, MemoryError> {
        for attempt in 0..=MAX_RETRIES {
            match self.try_acquire_once(entity_name).await? {
                true => return Ok(true),
                false if attempt == MAX_RETRIES => return Ok(false),
                false => {
                    let backoff_ms = BASE_BACKOFF_MS * (1u64 << attempt);
                    sleep(Duration::from_millis(backoff_ms)).await;
                }
            }
        }
        Ok(false)
    }

    async fn try_acquire_once(&self, entity_name: &str) -> Result<bool, MemoryError> {
        // INSERT OR IGNORE: succeeds if no row exists.
        // Then UPDATE: refreshes the lock if held by this session OR if it has expired.
        // A single round-trip via RETURNING id would be nicer but the expired-or-same-session
        // condition requires a WHERE clause that INSERT OR IGNORE cannot express.
        // We use a two-statement approach in a transaction for atomicity.

        let acquired: bool = query_scalar(sql!(
            "INSERT INTO entity_advisory_locks (entity_name, session_id, acquired_at, expires_at)
             VALUES (?, ?, datetime('now'), datetime('now', ? || ' seconds'))
             ON CONFLICT(entity_name) DO UPDATE SET
                 session_id  = excluded.session_id,
                 acquired_at = excluded.acquired_at,
                 expires_at  = excluded.expires_at
             WHERE
                 -- reclaim if expired
                 entity_advisory_locks.expires_at < datetime('now')
                 OR
                 -- refresh if same session
                 entity_advisory_locks.session_id = excluded.session_id
             RETURNING (session_id = ?) AS acquired"
        ))
        .bind(entity_name)
        .bind(self.session_id.as_str())
        .bind(LOCK_TTL_SECS.to_string())
        .bind(self.session_id.as_str())
        .fetch_optional(self.pool())
        .await?
        .unwrap_or(false);

        Ok(acquired)
    }

    /// Extend the TTL of a lock held by this session.
    ///
    /// Called before long operations (e.g., an LLM call inside entity resolution)
    /// to prevent the lock from expiring while work is in progress.
    ///
    /// Returns `true` if the lock was extended (still held by this session).
    ///
    /// # Errors
    ///
    /// Returns an error on database failures.
    pub async fn extend_lock(
        &self,
        entity_name: &str,
        extra_secs: i64,
    ) -> Result<bool, MemoryError> {
        let affected = query(sql!(
            "UPDATE entity_advisory_locks
             SET expires_at = datetime(expires_at, ? || ' seconds')
             WHERE entity_name = ? AND session_id = ?"
        ))
        .bind(extra_secs.to_string())
        .bind(entity_name)
        .bind(self.session_id.as_str())
        .execute(self.pool())
        .await?
        .rows_affected();

        Ok(affected > 0)
    }

    /// Release the lock on `entity_name` held by this session.
    ///
    /// No-op if the lock was already reclaimed by another session.
    ///
    /// # Errors
    ///
    /// Returns an error on database failures.
    pub async fn release(&self, entity_name: &str) -> Result<(), MemoryError> {
        query(sql!(
            "DELETE FROM entity_advisory_locks
             WHERE entity_name = ? AND session_id = ?"
        ))
        .bind(entity_name)
        .bind(self.session_id.as_str())
        .execute(self.pool())
        .await?;

        Ok(())
    }

    /// Release all locks held by this session.
    ///
    /// Called on agent shutdown to avoid leaving locks until TTL expiry.
    ///
    /// # Errors
    ///
    /// Returns an error on database failures.
    pub async fn release_all(&self) -> Result<(), MemoryError> {
        query(sql!(
            "DELETE FROM entity_advisory_locks WHERE session_id = ?"
        ))
        .bind(self.session_id.as_str())
        .execute(self.pool())
        .await?;

        Ok(())
    }

    fn pool(&self) -> &DbPool {
        &self.pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::DbStore;

    async fn make_lock_manager(session_id: &str) -> EntityLockManager {
        let store = DbStore::with_pool_size(":memory:", 1)
            .await
            .expect("in-memory store");
        EntityLockManager::new(store.pool().clone(), session_id)
    }

    async fn make_shared_managers(
        session_a: &str,
        session_b: &str,
    ) -> (EntityLockManager, EntityLockManager) {
        let store = DbStore::with_pool_size(":memory:", 2)
            .await
            .expect("in-memory store");
        let pool = store.pool().clone();
        (
            EntityLockManager::new(pool.clone(), session_a),
            EntityLockManager::new(pool, session_b),
        )
    }

    #[tokio::test]
    async fn try_acquire_succeeds_on_first_call() {
        let mgr = make_lock_manager("session-a").await;
        let acquired = mgr.try_acquire("entity::Foo").await.expect("try_acquire");
        assert!(acquired);
    }

    #[tokio::test]
    async fn try_acquire_same_session_refresh_succeeds() {
        let mgr = make_lock_manager("session-a").await;
        assert!(mgr.try_acquire("entity::Foo").await.expect("first"));
        // Same session — should refresh and return true immediately.
        assert!(mgr.try_acquire("entity::Foo").await.expect("second"));
    }

    #[tokio::test]
    async fn try_acquire_fails_when_held_by_different_session() {
        let (a, b) = make_shared_managers("session-a", "session-b").await;
        assert!(a.try_acquire("entity::Foo").await.expect("a acquires"));
        // Session B cannot acquire the same entity (will exhaust retries).
        // We use try_acquire_once directly via a fresh lock on an entity no one holds first,
        // then test contention by calling the public API.
        // Since MAX_RETRIES=3 with backoff, this adds ~350ms per test. Acceptable.
        let acquired = b.try_acquire("entity::Foo").await.expect("b tries");
        assert!(
            !acquired,
            "session-b should not acquire a lock held by session-a"
        );
    }

    #[tokio::test]
    async fn expired_lock_is_reclaimed_by_new_session() {
        let store = DbStore::with_pool_size(":memory:", 2)
            .await
            .expect("in-memory store");
        let pool = store.pool().clone();
        let b = EntityLockManager::new(pool.clone(), "session-b");
        // Insert an already-expired lock directly into the table.
        zeph_db::query(zeph_db::sql!(
            "INSERT INTO entity_advisory_locks (entity_name, session_id, acquired_at, expires_at)
             VALUES ('entity::Bar', 'session-a', datetime('now', '-200 seconds'), datetime('now', '-80 seconds'))"
        ))
        .execute(&pool)
        .await
        .expect("insert expired lock");

        // Session B should reclaim the expired lock.
        let acquired = b.try_acquire("entity::Bar").await.expect("try_acquire");
        assert!(acquired, "session-b should reclaim an expired lock");
    }

    #[tokio::test]
    async fn release_clears_the_lock() {
        let (a, b) = make_shared_managers("session-a", "session-b").await;
        a.try_acquire("entity::Baz").await.expect("acquire");
        a.release("entity::Baz").await.expect("release");

        // After release, a different session can immediately acquire (no retries needed).
        let acquired = b.try_acquire("entity::Baz").await.expect("b reacquire");
        assert!(acquired);
    }

    #[tokio::test]
    async fn release_is_noop_for_wrong_session() {
        let (a, b) = make_shared_managers("session-a", "session-b").await;
        assert!(a.try_acquire("entity::Qux").await.expect("a acquires"));
        // Session B releasing a lock it doesn't hold: should be a no-op.
        b.release("entity::Qux").await.expect("release noop");
        // Session A still holds the lock — B cannot acquire.
        let acquired = b.try_acquire("entity::Qux").await.expect("b tries");
        assert!(!acquired);
    }

    #[tokio::test]
    async fn release_all_removes_all_session_locks() {
        let mgr = make_lock_manager("session-a").await;
        mgr.try_acquire("entity::One").await.expect("one");
        mgr.try_acquire("entity::Two").await.expect("two");
        mgr.release_all().await.expect("release_all");

        // Both locks removed — can re-acquire immediately.
        assert!(mgr.try_acquire("entity::One").await.expect("re-one"));
        assert!(mgr.try_acquire("entity::Two").await.expect("re-two"));
    }

    #[tokio::test]
    async fn extend_lock_returns_true_for_owner() {
        let mgr = make_lock_manager("session-a").await;
        mgr.try_acquire("entity::Ext").await.expect("acquire");
        let extended = mgr.extend_lock("entity::Ext", 60).await.expect("extend");
        assert!(extended);
    }

    #[tokio::test]
    async fn extend_lock_returns_false_for_non_owner() {
        let (a, b) = make_shared_managers("session-a", "session-b").await;
        a.try_acquire("entity::Ext2").await.expect("a acquires");
        let extended = b.extend_lock("entity::Ext2", 60).await.expect("b extend");
        assert!(
            !extended,
            "non-owner session should not be able to extend lock"
        );
    }
}
