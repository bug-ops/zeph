// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! CRUD for the `agent_sessions` table used by the fleet dashboard (#3884).

use serde::{Deserialize, Serialize};
#[allow(unused_imports)]
use zeph_db::sql;

use super::SqliteStore;
use crate::error::MemoryError;

/// Discriminant for the type of agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum SessionKind {
    /// An interactive conversation session (CLI or TUI).
    Interactive,
    /// An autonomous goal-directed session.
    Autonomous,
    /// An ACP (agent-to-agent communication protocol) session.
    Acp,
}

impl SessionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Autonomous => "autonomous",
            Self::Acp => "acp",
        }
    }
}

impl std::fmt::Display for SessionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SessionKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "interactive" => Ok(Self::Interactive),
            "autonomous" => Ok(Self::Autonomous),
            "acp" => Ok(Self::Acp),
            other => Err(format!("unknown session kind: {other}")),
        }
    }
}

/// Lifecycle status of an agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum SessionStatus {
    /// Session is currently running.
    Active,
    /// Session ended normally.
    Completed,
    /// Session ended due to an unrecoverable error.
    Failed,
    /// Session was cancelled by the user.
    Cancelled,
    /// Session was active at a previous startup and presumed crashed (no clean shutdown recorded).
    Unknown,
}

impl SessionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SessionStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "unknown" => Ok(Self::Unknown),
            other => Err(format!("unknown session status: {other}")),
        }
    }
}

/// Channel over which the session was initiated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum SessionChannel {
    Cli,
    Tui,
    Telegram,
    Discord,
    Slack,
    Acp,
}

impl SessionChannel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Tui => "tui",
            Self::Telegram => "telegram",
            Self::Discord => "discord",
            Self::Slack => "slack",
            Self::Acp => "acp",
        }
    }
}

impl std::fmt::Display for SessionChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SessionChannel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "cli" => Ok(Self::Cli),
            "tui" => Ok(Self::Tui),
            "telegram" => Ok(Self::Telegram),
            "discord" => Ok(Self::Discord),
            "slack" => Ok(Self::Slack),
            "acp" => Ok(Self::Acp),
            other => Err(format!("unknown session channel: {other}")),
        }
    }
}

/// A row from the `agent_sessions` table.
///
/// Each field maps directly to a column. Token fields are informational; `reasoning_tokens`
/// is a subset of `completion_tokens` and must **not** be added separately to cost.
#[derive(Debug, Clone)]
pub struct AgentSessionRow {
    /// Stable session identifier (UUID).
    pub id: String,
    /// Session type discriminant.
    pub kind: SessionKind,
    /// Current lifecycle state.
    pub status: SessionStatus,
    /// Channel over which the session runs.
    pub channel: SessionChannel,
    /// LLM model name (e.g. `claude-sonnet-4-6`).
    pub model: String,
    /// ISO-8601 creation timestamp (UTC).
    pub created_at: String,
    /// ISO-8601 timestamp of the most recent agent turn (UTC).
    pub last_active_at: String,
    /// Number of completed agent turns.
    pub turns: u32,
    /// Prompt (input) tokens consumed across all turns.
    pub prompt_tokens: u64,
    /// Completion (output) tokens generated across all turns.
    pub completion_tokens: u64,
    /// Reasoning tokens (subset of `completion_tokens`; `OpenAI` only, others are 0).
    pub reasoning_tokens: u64,
    /// Estimated session cost in US cents.
    pub cost_cents: f64,
    /// Goal description for autonomous sessions; `None` for interactive sessions.
    pub goal_text: Option<String>,
}

impl SqliteStore {
    /// Insert or update an agent session row.
    ///
    /// On conflict on `id` the row is updated with the latest field values.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    #[tracing::instrument(name = "memory.fleet.upsert_session", skip_all, level = "debug", err)]
    pub async fn upsert_agent_session(&self, s: &AgentSessionRow) -> Result<(), MemoryError> {
        zeph_db::query(
            "INSERT INTO agent_sessions \
             (id, kind, status, channel, model, created_at, last_active_at, \
              turns, prompt_tokens, completion_tokens, reasoning_tokens, cost_cents, goal_text) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET \
               kind = excluded.kind, \
               status = excluded.status, \
               channel = excluded.channel, \
               model = excluded.model, \
               last_active_at = excluded.last_active_at, \
               turns = excluded.turns, \
               prompt_tokens = excluded.prompt_tokens, \
               completion_tokens = excluded.completion_tokens, \
               reasoning_tokens = excluded.reasoning_tokens, \
               cost_cents = excluded.cost_cents, \
               goal_text = excluded.goal_text",
        )
        .bind(&s.id)
        .bind(s.kind.as_str())
        .bind(s.status.as_str())
        .bind(s.channel.as_str())
        .bind(&s.model)
        .bind(&s.created_at)
        .bind(&s.last_active_at)
        // `turns` maps to an `INTEGER` column in both dialects; bind it as `i32` so the query
        // stays backend-agnostic. A bare `u32` only implements `Type<Sqlite>` (Postgres has no
        // unsigned integer types), which pins the query to the SQLite driver and breaks the
        // Postgres backend (#4964).
        .bind(s.turns.cast_signed())
        .bind(s.prompt_tokens.cast_signed())
        .bind(s.completion_tokens.cast_signed())
        .bind(s.reasoning_tokens.cast_signed())
        .bind(s.cost_cents)
        .bind(&s.goal_text)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update the status of a session by its ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    #[tracing::instrument(
        name = "memory.fleet.update_agent_session_status",
        skip_all,
        level = "debug",
        err
    )]
    pub async fn update_agent_session_status(
        &self,
        id: &str,
        status: SessionStatus,
    ) -> Result<(), MemoryError> {
        zeph_db::query(sql!(
            "UPDATE agent_sessions SET status = ?, last_active_at = CURRENT_TIMESTAMP WHERE id = ?"
        ))
        .bind(status.as_str())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark all sessions that are still `active` as `unknown` (crashed), except
    /// for the current session.
    ///
    /// Called on startup to reconcile stale sessions from a previous unclean shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    #[tracing::instrument(
        name = "memory.fleet.reconcile_stale_sessions",
        skip_all,
        level = "debug",
        err
    )]
    pub async fn reconcile_stale_sessions(
        &self,
        current_session_id: &str,
    ) -> Result<u64, MemoryError> {
        let result = zeph_db::query(sql!(
            "UPDATE agent_sessions SET status = 'unknown' \
             WHERE status = 'active' AND id != ?"
        ))
        .bind(current_session_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// List agent sessions ordered by `last_active_at` descending.
    ///
    /// Pass `status_filter = Some(status)` to restrict results to that lifecycle state.
    /// Pass `limit = 0` for unlimited results.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[tracing::instrument(name = "memory.fleet.list_sessions", skip_all, level = "debug", err)]
    pub async fn list_agent_sessions(
        &self,
        limit: u32,
        status_filter: Option<SessionStatus>,
    ) -> Result<Vec<AgentSessionRow>, MemoryError> {
        #[allow(clippy::cast_possible_wrap)]
        let sql_limit: i64 = if limit == 0 { -1 } else { i64::from(limit) };

        type SessionRow = (
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            // `turns` is an `INTEGER` column (INT4 on Postgres); decode it as `i32` so the
            // Postgres driver does not reject it as an `i64`/BIGINT mismatch (#4964).
            i32,
            i64,
            i64,
            i64,
            f64,
            Option<String>,
        );
        let rows: Vec<SessionRow> = if let Some(sf) = status_filter {
            zeph_db::query_as(
                "SELECT id, kind, status, channel, model, created_at, last_active_at, \
                 turns, prompt_tokens, completion_tokens, reasoning_tokens, cost_cents, goal_text \
                 FROM agent_sessions WHERE status = ? \
                 ORDER BY last_active_at DESC LIMIT ?",
            )
            .bind(sf.as_str())
            .bind(sql_limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            zeph_db::query_as(
                "SELECT id, kind, status, channel, model, created_at, last_active_at, \
                 turns, prompt_tokens, completion_tokens, reasoning_tokens, cost_cents, goal_text \
                 FROM agent_sessions \
                 ORDER BY last_active_at DESC LIMIT ?",
            )
            .bind(sql_limit)
            .fetch_all(&self.pool)
            .await?
        };

        Ok(rows
            .into_iter()
            .map(
                |(
                    id,
                    kind_s,
                    status_s,
                    channel_s,
                    model,
                    created_at,
                    last_active_at,
                    turns,
                    prompt_tokens,
                    completion_tokens,
                    reasoning_tokens,
                    cost_cents,
                    goal_text,
                )| {
                    AgentSessionRow {
                        id,
                        kind: kind_s.parse().unwrap_or(SessionKind::Interactive),
                        status: status_s.parse().unwrap_or(SessionStatus::Unknown),
                        channel: channel_s.parse().unwrap_or(SessionChannel::Cli),
                        model,
                        created_at,
                        last_active_at,
                        turns: u32::try_from(turns).unwrap_or(0),
                        prompt_tokens: u64::try_from(prompt_tokens).unwrap_or(0),
                        completion_tokens: u64::try_from(completion_tokens).unwrap_or(0),
                        reasoning_tokens: u64::try_from(reasoning_tokens).unwrap_or(0),
                        cost_cents,
                        goal_text,
                    }
                },
            )
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn make_store() -> SqliteStore {
        SqliteStore::new(":memory:")
            .await
            .expect("SqliteStore::new")
    }

    fn sample(id: &str) -> AgentSessionRow {
        AgentSessionRow {
            id: id.to_owned(),
            kind: SessionKind::Interactive,
            status: SessionStatus::Active,
            channel: SessionChannel::Cli,
            model: "claude-sonnet-4-6".to_owned(),
            created_at: "2026-01-01T00:00:00".to_owned(),
            last_active_at: "2026-01-01T00:00:00".to_owned(),
            turns: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            reasoning_tokens: 0,
            cost_cents: 0.0,
            goal_text: None,
        }
    }

    #[tokio::test]
    async fn upsert_and_list() {
        let store = make_store().await;
        store.upsert_agent_session(&sample("s1")).await.unwrap();
        let rows = store.list_agent_sessions(10, None).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "s1");
        assert_eq!(rows[0].kind, SessionKind::Interactive);
        assert_eq!(rows[0].status, SessionStatus::Active);
    }

    #[tokio::test]
    async fn upsert_updates_existing() {
        let store = make_store().await;
        store.upsert_agent_session(&sample("s1")).await.unwrap();
        let mut updated = sample("s1");
        updated.turns = 5;
        updated.status = SessionStatus::Completed;
        store.upsert_agent_session(&updated).await.unwrap();

        let rows = store.list_agent_sessions(10, None).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].turns, 5);
        assert_eq!(rows[0].status, SessionStatus::Completed);
    }

    #[tokio::test]
    async fn update_status() {
        let store = make_store().await;
        store.upsert_agent_session(&sample("s1")).await.unwrap();
        store
            .update_agent_session_status("s1", SessionStatus::Failed)
            .await
            .unwrap();
        let rows = store.list_agent_sessions(10, None).await.unwrap();
        assert_eq!(rows[0].status, SessionStatus::Failed);
    }

    #[tokio::test]
    async fn reconcile_stale_sessions() {
        let store = make_store().await;
        store.upsert_agent_session(&sample("s1")).await.unwrap();
        store.upsert_agent_session(&sample("s2")).await.unwrap();
        let affected = store.reconcile_stale_sessions("s2").await.unwrap();
        assert_eq!(affected, 1);

        let rows = store.list_agent_sessions(10, None).await.unwrap();
        let s1 = rows.iter().find(|r| r.id == "s1").unwrap();
        let s2 = rows.iter().find(|r| r.id == "s2").unwrap();
        assert_eq!(s1.status, SessionStatus::Unknown);
        assert_eq!(s2.status, SessionStatus::Active);
    }

    #[tokio::test]
    async fn list_with_status_filter() {
        let store = make_store().await;
        store.upsert_agent_session(&sample("s1")).await.unwrap();
        let mut s2 = sample("s2");
        s2.status = SessionStatus::Completed;
        store.upsert_agent_session(&s2).await.unwrap();

        let active = store
            .list_agent_sessions(10, Some(SessionStatus::Active))
            .await
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "s1");
    }

    #[tokio::test]
    async fn list_respects_limit() {
        let store = make_store().await;
        for i in 0..5u8 {
            store
                .upsert_agent_session(&sample(&format!("s{i}")))
                .await
                .unwrap();
        }
        let rows = store.list_agent_sessions(3, None).await.unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[tokio::test]
    async fn session_kind_roundtrip() {
        use std::str::FromStr as _;
        assert_eq!(
            SessionKind::from_str("autonomous").unwrap(),
            SessionKind::Autonomous
        );
        assert!(SessionKind::from_str("bad").is_err());
    }

    #[tokio::test]
    async fn session_status_unknown_roundtrip() {
        use std::str::FromStr as _;
        assert_eq!(
            SessionStatus::from_str("unknown").unwrap(),
            SessionStatus::Unknown
        );
    }
}
