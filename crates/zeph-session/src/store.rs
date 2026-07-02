// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`SessionStore`]: the `acp_sessions` metadata index.
//!
//! Promotes the existing `acp_sessions` table (migration 013, `crates/zeph-memory`) to a
//! channel-agnostic conversation-session index (spec-068 §2 Decision D1 — no new `sessions`
//! table is introduced). `zeph-session` talks to the table directly via [`zeph_db::DbPool`]
//! rather than depending on `zeph-memory`, keeping the crate boundary intact.
//!
//! The event log ([`crate::log::SessionEventLog`]) is the source of truth for conversation
//! content; this store only tracks lightweight, queryable metadata (`last_seq`, `status`,
//! fork provenance) used to reconcile the projection on open (INV-SP-3) and to answer
//! `sessions list` without replaying every log.

use zeph_db::{ActiveDialect, DbPool, dialect::Dialect, sql};

use crate::error::SessionError;

/// Lifecycle status of a conversation-session, mirroring the `acp_sessions.status` CHECK
/// constraint added in migration 106.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Actively attached to a live agent/actor.
    Active,
    /// Persisted but not currently attached.
    Idle,
    /// Explicitly archived; excluded from default `list` results.
    Archived,
}

impl SessionStatus {
    /// The `TEXT` representation stored in the `status` column.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Idle => "idle",
            Self::Archived => "archived",
        }
    }
}

impl std::str::FromStr for SessionStatus {
    type Err = SessionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "idle" => Ok(Self::Idle),
            "archived" => Ok(Self::Archived),
            other => Err(SessionError::NotFound(format!(
                "unknown session status: {other}"
            ))),
        }
    }
}

/// A conversation-session's metadata row, as tracked in `acp_sessions`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionMetadata {
    pub session_id: String,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub conversation_id: Option<i64>,
    pub last_seq: u64,
    pub event_count: u64,
    pub forked_from: Option<String>,
    pub forked_at_seq: Option<u64>,
    pub status: SessionStatus,
    pub last_condensed_seq: u64,
}

/// Filter parameters for [`SessionStore::list`].
#[derive(Debug, Clone, Default)]
pub struct SessionFilter {
    /// Restrict to a single status; `None` returns all statuses.
    pub status: Option<SessionStatus>,
    /// Maximum rows returned; `0` means unlimited.
    pub limit: usize,
}

/// CRUD access to the `acp_sessions` metadata index.
pub struct SessionStore {
    pool: DbPool,
}

impl SessionStore {
    /// Wrap an existing [`DbPool`]. `zeph-session` does not own a dedicated database file —
    /// it shares the pool that already owns `acp_sessions` (migration 013).
    #[must_use]
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Insert a new session row with `status = 'active'`, ignoring the call if the row already
    /// exists (idempotent, mirrors the existing `create_acp_session` pattern).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Db`] if the write fails.
    #[tracing::instrument(name = "session.store.create", skip_all, level = "debug")]
    pub async fn create(&self, session_id: &str) -> Result<(), SessionError> {
        let stmt = zeph_db::rewrite_placeholders(&format!(
            "{} INTO acp_sessions (id, status) VALUES (?, 'active'){}",
            <ActiveDialect as Dialect>::INSERT_IGNORE,
            <ActiveDialect as Dialect>::CONFLICT_NOTHING,
        ));
        zeph_db::query(sqlx::AssertSqlSafe(stmt))
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Update `last_seq`, `event_count`, and `updated_at` after a turn's events are flushed to
    /// the log (INV-SP-1: called only after the log append is durable).
    ///
    /// Explicitly bumps `updated_at` here because the pre-cutover `AFTER INSERT ON
    /// acp_session_events` trigger (migration 017) that used to drive it never fires for
    /// post-cutover sessions (spec-068 §12.3 / D-2: `acp_session_events` is a write target only
    /// for legacy pre-cutover sessions) — without this, `list_acp_sessions`' "ordered by last
    /// activity descending" would silently degrade to "ordered by creation time" for every
    /// session created after the cutover.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Db`] if the write fails.
    #[allow(clippy::cast_possible_wrap)]
    #[tracing::instrument(name = "session.store.update_seq", skip_all, level = "debug")]
    pub async fn update_seq(
        &self,
        session_id: &str,
        last_seq: u64,
        event_count: u64,
    ) -> Result<(), SessionError> {
        let stmt = zeph_db::rewrite_placeholders(&format!(
            "UPDATE acp_sessions SET last_seq = ?, event_count = ?, updated_at = {} WHERE id = ?",
            <ActiveDialect as Dialect>::NOW,
        ));
        zeph_db::query(sqlx::AssertSqlSafe(stmt))
            .bind(last_seq as i64)
            .bind(event_count as i64)
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Update the session's lifecycle status.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Db`] if the write fails.
    #[tracing::instrument(name = "session.store.set_status", skip_all, level = "debug")]
    pub async fn set_status(
        &self,
        session_id: &str,
        status: SessionStatus,
    ) -> Result<(), SessionError> {
        zeph_db::query(sql!("UPDATE acp_sessions SET status = ? WHERE id = ?"))
            .bind(status.as_str())
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Update the high-water condensation mark (INV-SP-4 non-overlap tracking).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Db`] if the write fails.
    #[allow(clippy::cast_possible_wrap)]
    #[tracing::instrument(name = "session.store.set_condensed_seq", skip_all, level = "debug")]
    pub async fn set_condensed_seq(
        &self,
        session_id: &str,
        last_condensed_seq: u64,
    ) -> Result<(), SessionError> {
        zeph_db::query(sql!(
            "UPDATE acp_sessions SET last_condensed_seq = ? WHERE id = ?"
        ))
        .bind(last_condensed_seq as i64)
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch a single session's metadata.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Db`] if the query fails.
    #[tracing::instrument(name = "session.store.get", skip_all, level = "debug")]
    pub async fn get(&self, session_id: &str) -> Result<Option<SessionMetadata>, SessionError> {
        let row = zeph_db::query_as::<_, SessionRow>(sql!(
            "SELECT id, title, created_at, updated_at, conversation_id, last_seq, event_count, \
             forked_from, forked_at_seq, status, last_condensed_seq \
             FROM acp_sessions WHERE id = ?"
        ))
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// List sessions, most recently updated first.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Db`] if the query fails.
    #[tracing::instrument(name = "session.store.list", skip_all, level = "debug")]
    pub async fn list(&self, filter: &SessionFilter) -> Result<Vec<SessionMetadata>, SessionError> {
        #[allow(clippy::cast_possible_wrap)]
        let sql_limit: i64 = if filter.limit == 0 {
            -1
        } else {
            filter.limit as i64
        };
        let status_filter = filter.status.map(SessionStatus::as_str);

        let rows = zeph_db::query_as::<_, SessionRow>(sql!(
            "SELECT id, title, created_at, updated_at, conversation_id, last_seq, event_count, \
             forked_from, forked_at_seq, status, last_condensed_seq \
             FROM acp_sessions \
             WHERE (? IS NULL OR status = ?) \
             ORDER BY updated_at DESC LIMIT ?"
        ))
        .bind(status_filter)
        .bind(status_filter)
        .bind(sql_limit)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// Link this session to a `ConversationId` (raw `i64` — `zeph-session` does not depend on
    /// `zeph-memory`'s newtype), enforcing the `SessionId`<->`ConversationId` bijection (spec
    /// §5.2) via the unique partial index added in migration 106.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Db`] if the write fails (including a unique-constraint violation
    /// when `conversation_id` is already linked to a different session).
    #[tracing::instrument(name = "session.store.link_conversation", skip_all, level = "debug")]
    pub async fn link_conversation(
        &self,
        session_id: &str,
        conversation_id: i64,
    ) -> Result<(), SessionError> {
        zeph_db::query(sql!(
            "UPDATE acp_sessions SET conversation_id = ? WHERE id = ?"
        ))
        .bind(conversation_id)
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Look up the session already linked to a `ConversationId`, if any.
    ///
    /// Used at non-ACP channel startup (CLI/TUI/Telegram) to resume the same conversation's
    /// existing session (and its event log) across process restarts, rather than minting a new
    /// `SessionId` every launch (spec §12.2).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Db`] if the query fails.
    #[tracing::instrument(
        name = "session.store.get_by_conversation_id",
        skip_all,
        level = "debug"
    )]
    pub async fn get_by_conversation_id(
        &self,
        conversation_id: i64,
    ) -> Result<Option<SessionMetadata>, SessionError> {
        let row = zeph_db::query_as::<_, SessionRow>(sql!(
            "SELECT id, title, created_at, updated_at, conversation_id, last_seq, event_count, \
             forked_from, forked_at_seq, status, last_condensed_seq \
             FROM acp_sessions WHERE conversation_id = ?"
        ))
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// Record a fork: sets `forked_from`/`forked_at_seq` on the child row.
    ///
    /// Does not touch the parent's log (the `ForkPoint` provenance event is appended by
    /// [`crate::replay`]'s `ForkEngine`, which owns the parent's [`crate::log::SessionEventLog`]).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Db`] if either write fails.
    #[allow(clippy::cast_possible_wrap)]
    #[tracing::instrument(name = "session.store.record_fork", skip_all, level = "debug")]
    pub async fn record_fork(
        &self,
        new_session_id: &str,
        src_session_id: &str,
        forked_at_seq: u64,
    ) -> Result<(), SessionError> {
        let stmt = zeph_db::rewrite_placeholders(&format!(
            "{} INTO acp_sessions (id, status, forked_from, forked_at_seq) VALUES (?, 'active', ?, ?){}",
            <ActiveDialect as Dialect>::INSERT_IGNORE,
            <ActiveDialect as Dialect>::CONFLICT_NOTHING,
        ));
        zeph_db::query(sqlx::AssertSqlSafe(stmt))
            .bind(new_session_id)
            .bind(src_session_id)
            .bind(forked_at_seq as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Delete a session's metadata row. Returns `true` if a row was deleted.
    ///
    /// Does not remove the on-disk event log directory or blobs — callers with access to
    /// `[session] data_dir` are responsible for that (mirrors the separation of concerns between
    /// [`SessionStore`] and [`crate::log::SessionEventLog`]).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Db`] if the write fails.
    #[tracing::instrument(name = "session.store.delete", skip_all, level = "debug")]
    pub async fn delete(&self, session_id: &str) -> Result<bool, SessionError> {
        let result = zeph_db::query(sql!("DELETE FROM acp_sessions WHERE id = ?"))
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[derive(sqlx::FromRow)]
struct SessionRow {
    id: String,
    title: Option<String>,
    created_at: String,
    updated_at: String,
    conversation_id: Option<i64>,
    last_seq: i64,
    event_count: i64,
    forked_from: Option<String>,
    forked_at_seq: Option<i64>,
    status: String,
    last_condensed_seq: i64,
}

impl TryFrom<SessionRow> for SessionMetadata {
    type Error = SessionError;

    fn try_from(row: SessionRow) -> Result<Self, Self::Error> {
        Ok(Self {
            session_id: row.id,
            title: row.title,
            created_at: row.created_at,
            updated_at: row.updated_at,
            conversation_id: row.conversation_id,
            last_seq: u64::try_from(row.last_seq).unwrap_or(0),
            event_count: u64::try_from(row.event_count).unwrap_or(0),
            forked_from: row.forked_from,
            forked_at_seq: row.forked_at_seq.map(|v| u64::try_from(v).unwrap_or(0)),
            status: row.status.parse()?,
            last_condensed_seq: u64::try_from(row.last_condensed_seq).unwrap_or(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn make_pool() -> DbPool {
        let config = zeph_db::DbConfig {
            url: ":memory:".to_owned(),
            ..Default::default()
        };
        let pool = config
            .connect()
            .await
            .expect("connect in-memory sqlite pool");
        zeph_db::run_migrations(&pool)
            .await
            .expect("run migrations");
        pool
    }

    #[tokio::test]
    async fn test_migration_106_idempotent() {
        let pool = make_pool().await;
        zeph_db::run_migrations(&pool)
            .await
            .expect("second run is a no-op");
    }

    #[tokio::test]
    async fn create_and_get_defaults() {
        let store = SessionStore::new(make_pool().await);
        store.create("s1").await.unwrap();
        let meta = store.get("s1").await.unwrap().expect("row exists");
        assert_eq!(meta.session_id, "s1");
        assert_eq!(meta.last_seq, 0);
        assert_eq!(meta.event_count, 0);
        assert_eq!(meta.status, SessionStatus::Active);
        assert!(meta.forked_from.is_none());
    }

    #[tokio::test]
    async fn update_seq_persists() {
        let store = SessionStore::new(make_pool().await);
        store.create("s1").await.unwrap();
        store.update_seq("s1", 41, 20).await.unwrap();
        let meta = store.get("s1").await.unwrap().unwrap();
        assert_eq!(meta.last_seq, 41);
        assert_eq!(meta.event_count, 20);
    }

    #[tokio::test]
    async fn set_status_persists() {
        let store = SessionStore::new(make_pool().await);
        store.create("s1").await.unwrap();
        store.set_status("s1", SessionStatus::Idle).await.unwrap();
        let meta = store.get("s1").await.unwrap().unwrap();
        assert_eq!(meta.status, SessionStatus::Idle);
    }

    #[tokio::test]
    async fn record_fork_sets_provenance() {
        let store = SessionStore::new(make_pool().await);
        store.create("parent").await.unwrap();
        store.record_fork("child", "parent", 12).await.unwrap();
        let meta = store.get("child").await.unwrap().unwrap();
        assert_eq!(meta.forked_from.as_deref(), Some("parent"));
        assert_eq!(meta.forked_at_seq, Some(12));
    }

    #[tokio::test]
    async fn list_filters_by_status() {
        let store = SessionStore::new(make_pool().await);
        store.create("s1").await.unwrap();
        store.create("s2").await.unwrap();
        store
            .set_status("s2", SessionStatus::Archived)
            .await
            .unwrap();

        let active = store
            .list(&SessionFilter {
                status: Some(SessionStatus::Active),
                limit: 0,
            })
            .await
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].session_id, "s1");

        let all = store.list(&SessionFilter::default()).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn delete_removes_row() {
        let store = SessionStore::new(make_pool().await);
        store.create("s1").await.unwrap();
        assert!(store.delete("s1").await.unwrap());
        assert!(store.get("s1").await.unwrap().is_none());
        assert!(!store.delete("s1").await.unwrap());
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let store = SessionStore::new(make_pool().await);
        assert!(store.get("no-such").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn link_conversation_and_lookup_round_trips() {
        let pool = make_pool().await;
        let store = SessionStore::new(pool.clone());
        store.create("s1").await.unwrap();

        // `conversation_id` carries an FK to `conversations(id)` (migration 001); insert a row
        // directly since creating conversations is zeph-memory's domain, out of scope here.
        let (cid,): (i64,) =
            zeph_db::query_as("INSERT INTO conversations DEFAULT VALUES RETURNING id")
                .fetch_one(&pool)
                .await
                .unwrap();

        store.link_conversation("s1", cid).await.unwrap();

        let meta = store.get("s1").await.unwrap().unwrap();
        assert_eq!(meta.conversation_id, Some(cid));

        let found = store.get_by_conversation_id(cid).await.unwrap().unwrap();
        assert_eq!(found.session_id, "s1");
    }

    #[tokio::test]
    async fn get_by_conversation_id_returns_none_when_unlinked() {
        let store = SessionStore::new(make_pool().await);
        store.create("s1").await.unwrap();
        assert!(store.get_by_conversation_id(99).await.unwrap().is_none());
    }
}
