// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Fleet session lifecycle management (#4356).
//!
//! Provides helpers for registering, updating, and reconciling agent sessions
//! in the `agent_sessions` table so the fleet dashboard has accurate data.

use std::pin::Pin;
use std::sync::Arc;

use zeph_memory::store::SqliteStore;
use zeph_memory::store::agent_sessions::{
    AgentSessionRow, SessionChannel, SessionKind, SessionStatus,
};
use zeph_subagent::fleet::{FleetRegistry, FleetSessionInfo, FleetSessionStatus};

/// Adapts [`SqliteStore`] to the [`FleetRegistry`] trait used by `SubAgentManager`.
///
/// Wrap a `SqliteStore` with this adapter and inject it via
/// `SubAgentManager::set_fleet_registry` so spawned sub-agents appear in the
/// fleet dashboard.
pub(crate) struct SqliteFleetRegistry(Arc<SqliteStore>);

impl SqliteFleetRegistry {
    /// Create a new adapter from a shared store.
    pub(crate) fn new(store: Arc<SqliteStore>) -> Self {
        Self(store)
    }
}

impl FleetRegistry for SqliteFleetRegistry {
    fn register_active<'a>(
        &'a self,
        info: &'a FleetSessionInfo,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let row = AgentSessionRow {
                id: info.id.clone(),
                kind: SessionKind::Autonomous,
                status: SessionStatus::Active,
                channel: SessionChannel::Cli,
                model: String::new(),
                created_at: info.started_at.clone(),
                last_active_at: info.started_at.clone(),
                turns: 0,
                prompt_tokens: 0,
                completion_tokens: 0,
                reasoning_tokens: 0,
                cost_cents: 0.0,
                goal_text: Some(format!("sub-agent: {}", info.agent_name)),
            };
            self.0
                .upsert_agent_session(&row)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn mark_terminal<'a>(
        &'a self,
        session_id: &'a str,
        status: FleetSessionStatus,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let s = match status {
                FleetSessionStatus::Completed => SessionStatus::Completed,
                FleetSessionStatus::Failed => SessionStatus::Failed,
                FleetSessionStatus::Cancelled => SessionStatus::Cancelled,
            };
            self.0
                .update_agent_session_status(session_id, s)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

/// Register a new session in the fleet table and reconcile any stale sessions.
///
/// Call this once at agent startup, after the `SqliteStore` is available and the
/// channel name is known. On unclean shutdown of a previous instance any
/// sessions that were still marked `active` are transitioned to `unknown`.
///
/// # Errors
///
/// Returns an error if the database writes fail. Non-fatal: callers should log
/// and continue — the fleet panel will simply lack data for this session.
#[tracing::instrument(name = "fleet.session.start", skip_all, fields(session_id, channel = channel_name, model))]
pub(crate) async fn start_session(
    sqlite: &SqliteStore,
    session_id: &str,
    channel_name: &str,
    model: &str,
) -> anyhow::Result<()> {
    match sqlite.reconcile_stale_sessions(session_id).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(count = n, "fleet: reconciled stale sessions"),
        Err(e) => tracing::warn!(error = %e, "fleet: reconcile_stale_sessions failed"),
    }

    let channel = parse_channel(channel_name);
    let row = AgentSessionRow {
        id: session_id.to_owned(),
        kind: SessionKind::Interactive,
        status: SessionStatus::Active,
        channel,
        model: model.to_owned(),
        created_at: utc_now(),
        last_active_at: utc_now(),
        turns: 0,
        prompt_tokens: 0,
        completion_tokens: 0,
        reasoning_tokens: 0,
        cost_cents: 0.0,
        goal_text: None,
    };
    sqlite
        .upsert_agent_session(&row)
        .await
        .map_err(|e| anyhow::anyhow!("fleet: upsert_agent_session failed: {e}"))?;

    tracing::debug!(
        session_id,
        channel = channel_name,
        model,
        "fleet: session registered"
    );
    Ok(())
}

/// Update the fleet session status on agent exit.
///
/// Call this after `agent.run()` returns, passing the result to derive the
/// appropriate terminal status.
#[tracing::instrument(name = "fleet.session.end", skip_all, fields(session_id))]
pub(crate) async fn end_session(
    sqlite: &SqliteStore,
    session_id: &str,
    run_result: &anyhow::Result<()>,
) {
    let status = if run_result.is_ok() {
        SessionStatus::Completed
    } else {
        SessionStatus::Failed
    };
    if let Err(e) = sqlite.update_agent_session_status(session_id, status).await {
        tracing::warn!(error = %e, session_id, "fleet: update_agent_session_status failed");
    }
}

fn parse_channel(name: &str) -> SessionChannel {
    match name {
        "tui" => SessionChannel::Tui,
        "telegram" => SessionChannel::Telegram,
        "discord" => SessionChannel::Discord,
        "slack" => SessionChannel::Slack,
        _ => SessionChannel::Cli,
    }
}

fn utc_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Format as SQLite-compatible ISO-8601 UTC string (no sub-second precision needed).
    let (year, month, day, hour, min, sec) = epoch_to_datetime(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}")
}

fn epoch_to_datetime(epoch: u64) -> (u64, u64, u64, u64, u64, u64) {
    let sec = epoch % 60;
    let epoch = epoch / 60;
    let min = epoch % 60;
    let epoch = epoch / 60;
    let hour = epoch % 24;
    let mut days = epoch / 24;

    // Days since 1970-01-01.
    let mut year = 1970u64;
    loop {
        let leap = is_leap(year);
        let days_in_year = if leap { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }

    let leap = is_leap(year);
    let months = [
        31u64,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1u64;
    for &m in &months {
        if days < m {
            break;
        }
        days -= m;
        month += 1;
    }

    (year, month, days + 1, hour, min, sec)
}

fn is_leap(year: u64) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}
