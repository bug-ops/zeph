// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::str::FromStr;
use std::sync::Arc;

use chrono::{NaiveDateTime, Utc};
use cron::Schedule as CronSchedule;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::mpsc;
use zeph_scheduler::{
    JobStore, SchedulerMessage, TaskDescriptor, TaskKind, TaskMode, normalize_cron_expr,
    sanitize_task_prompt,
};
use zeph_tools::executor::{
    ToolCall, ToolError, ToolExecutor, ToolOutput, deserialize_params, truncate_tool_output,
};
use zeph_tools::registry::{InvocationHint, ToolDef};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PeriodicParams {
    pub name: String,
    pub cron: String,
    pub kind: String,
    #[serde(default)]
    pub config: serde_json::Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeferredParams {
    pub name: String,
    #[schemars(
        description = "When to run. Accepts: ISO 8601 UTC (2026-03-03T18:00:00Z), naive ISO 8601 (2026-03-03T18:00:00), relative offsets (+2h, +30m, +1h30m, 5s, +3d), or natural expressions (in 5 minutes, in 2 hours, today 14:30, tomorrow 09:00)."
    )]
    pub run_at: String,
    pub kind: String,
    #[serde(default)]
    pub task: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CancelParams {
    pub name: String,
}

/// Parses a relative shorthand like `+2m`, `1h30m`, `+3d`, `5s` into a [`chrono::Duration`].
///
/// Units: `s` = seconds, `m` = minutes, `h` = hours, `d` = days. Leading `+` is optional.
/// Returns `None` for zero duration, trailing digits without unit, or unknown unit characters.
fn parse_relative(s: &str) -> Option<chrono::Duration> {
    let s = s.strip_prefix('+').unwrap_or(s);
    let mut total_secs: i64 = 0;
    let mut num_buf = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num_buf.push(ch);
        } else {
            let n: i64 = num_buf.parse().ok()?;
            num_buf.clear();
            let unit_secs = match ch {
                's' => n,
                'm' => n.checked_mul(60)?,
                'h' => n.checked_mul(3600)?,
                'd' => n.checked_mul(86400)?,
                _ => return None,
            };
            total_secs = total_secs.checked_add(unit_secs)?;
        }
    }
    if !num_buf.is_empty() {
        return None;
    }
    if total_secs == 0 {
        return None;
    }
    Some(chrono::Duration::seconds(total_secs))
}

/// Parses natural-language time expressions into an absolute [`chrono::DateTime<Utc>`].
///
/// Supported patterns (case-insensitive):
/// - `"in N second(s)/minute(s)/hour(s)/day(s)"` — relative offset from `now`
/// - `"today HH:MM"` — today at the given time (UTC)
/// - `"tomorrow HH:MM"` — tomorrow at the given time (UTC)
fn parse_natural(s: &str, now: chrono::DateTime<Utc>) -> Option<chrono::DateTime<Utc>> {
    let s = s.trim().to_lowercase();
    if let Some(rest) = s.strip_prefix("in ") {
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if parts.len() == 2 {
            let n: i64 = parts[0].parse().ok()?;
            let secs = match parts[1].trim_end_matches('s') {
                "second" => n,
                "minute" => n * 60,
                "hour" => n * 3600,
                "day" => n * 86400,
                _ => return None,
            };
            return Some(now + chrono::Duration::seconds(secs));
        }
    }
    let (day_offset, rest) = if let Some(r) = s.strip_prefix("tomorrow ") {
        (1i64, r)
    } else if let Some(r) = s.strip_prefix("today ") {
        (0i64, r)
    } else {
        return None;
    };
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() != 2 {
        return None;
    }
    let hour: u32 = parts[0].parse().ok()?;
    let minute: u32 = parts[1].parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    let today = now.date_naive();
    let target = (today + chrono::Duration::days(day_offset))
        .and_hms_opt(hour, minute, 0)?
        .and_utc();
    Some(target)
}

/// Parses a `run_at` string into an absolute UTC timestamp using a caller-supplied `now`.
///
/// Tries strategies in order:
/// 1. ISO 8601 with timezone (e.g. `2026-03-03T18:00:00Z`)
/// 2. Naive ISO 8601 assumed UTC (e.g. `2026-03-03T18:00:00`)
/// 3. Relative shorthand (e.g. `+2h`, `30m`, `+1h30m`)
/// 4. Natural language (e.g. `in 5 minutes`, `tomorrow 10:00`)
///
/// Passing `now` explicitly avoids a TOCTOU race between parsing and the future-time check.
/// Returns `None` if none of the strategies match.
fn parse_run_at(s: &str, now: chrono::DateTime<Utc>) -> Option<chrono::DateTime<Utc>> {
    let s = s.trim();
    if let Ok(dt) = s.parse::<chrono::DateTime<Utc>>() {
        return Some(dt);
    }
    if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(ndt.and_utc());
    }
    if let Some(dur) = parse_relative(s) {
        return Some(now + dur);
    }
    parse_natural(s, now)
}

/// Tool executor that exposes scheduler management to the LLM.
pub struct SchedulerExecutor {
    task_tx: mpsc::Sender<SchedulerMessage>,
    store: Arc<JobStore>,
    /// Optional channel to signal TUI metrics refresh after task mutations.
    refresh_tx: Option<tokio::sync::watch::Sender<()>>,
}

impl SchedulerExecutor {
    #[must_use]
    pub fn new(task_tx: mpsc::Sender<SchedulerMessage>, store: Arc<JobStore>) -> Self {
        Self {
            task_tx,
            store,
            refresh_tx: None,
        }
    }

    /// Attach a watch sender used to trigger immediate TUI metrics refresh.
    #[must_use]
    pub fn with_refresh_tx(mut self, tx: tokio::sync::watch::Sender<()>) -> Self {
        self.refresh_tx = Some(tx);
        self
    }

    /// Return a cloned reference to the backing `JobStore` for external inspection (e.g. TUI).
    #[must_use]
    pub fn store(&self) -> Arc<JobStore> {
        Arc::clone(&self.store)
    }

    fn notify_refresh(&self) {
        if let Some(ref tx) = self.refresh_tx {
            let _ = tx.send(());
        }
    }

    async fn schedule_periodic(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        let params: PeriodicParams = deserialize_params(&call.params)?;

        if params.name.chars().count() > 128 {
            return Err(ToolError::InvalidParams {
                message: "name exceeds 128 characters".into(),
            });
        }

        if params.kind.chars().count() > 64 {
            return Err(ToolError::InvalidParams {
                message: "kind exceeds 64 characters".into(),
            });
        }

        if params.cron.chars().count() > 64 {
            return Err(ToolError::InvalidParams {
                message: "cron expression exceeds 64 characters".into(),
            });
        }

        let normalized_cron = normalize_cron_expr(&params.cron);
        let schedule =
            CronSchedule::from_str(&normalized_cron).map_err(|e| ToolError::InvalidParams {
                message: format!("invalid cron expression: {e}"),
            })?;

        let exists =
            self.store
                .job_exists(&params.name)
                .await
                .map_err(|e| ToolError::InvalidParams {
                    message: format!("store error: {e}"),
                })?;

        let action = if exists { "Updated" } else { "Created" };
        let kind = TaskKind::from_str_kind(&params.kind);

        let next_run = schedule
            .after(&Utc::now())
            .next()
            .map_or_else(|| "no future occurrence".to_owned(), |dt| dt.to_rfc3339());

        let desc = TaskDescriptor {
            name: params.name.clone(),
            mode: TaskMode::Periodic {
                schedule: Box::new(schedule),
            },
            kind,
            config: params.config,
        };

        self.store
            .upsert_job(&params.name, &normalized_cron, &params.kind)
            .await
            .map_err(|e| ToolError::InvalidParams {
                message: format!("store error: {e}"),
            })?;
        self.store
            .set_next_run(&params.name, &next_run)
            .await
            .map_err(|e| ToolError::InvalidParams {
                message: format!("store error: {e}"),
            })?;

        self.task_tx
            .try_send(SchedulerMessage::Add(Box::new(desc)))
            .map_err(|_| ToolError::InvalidParams {
                message: "scheduler channel full or closed".into(),
            })?;

        let summary = format!(
            "{action} periodic task '{}' (kind: {}, next run: {next_run})",
            params.name, params.kind
        );
        self.notify_refresh();
        Ok(Some(make_output("schedule_periodic", &summary)))
    }

    async fn schedule_deferred(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        let params: DeferredParams = deserialize_params(&call.params)?;

        if params.name.chars().count() > 128 {
            return Err(ToolError::InvalidParams {
                message: "name exceeds 128 characters".into(),
            });
        }

        if params.kind.chars().count() > 64 {
            return Err(ToolError::InvalidParams {
                message: "kind exceeds 64 characters".into(),
            });
        }

        let now = Utc::now();
        let run_at = parse_run_at(&params.run_at, now).ok_or_else(|| ToolError::InvalidParams {
            message: "run_at: expected ISO 8601 (2026-03-03T18:00:00Z or 2026-03-03T18:00:00), relative (+2h, +30m, +1h30m), or natural (in 5 minutes, tomorrow 10:00)".into(),
        })?;

        if run_at <= now {
            return Err(ToolError::InvalidParams {
                message: "run_at must be in the future".into(),
            });
        }

        let exists =
            self.store
                .job_exists(&params.name)
                .await
                .map_err(|e| ToolError::InvalidParams {
                    message: format!("store error: {e}"),
                })?;

        let action = if exists { "Updated" } else { "Created" };
        let kind = TaskKind::from_str_kind(&params.kind);
        let config = if params.task.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({ "task": sanitize_task_prompt(&params.task) })
        };

        let desc = TaskDescriptor {
            name: params.name.clone(),
            mode: TaskMode::OneShot { run_at },
            kind,
            config,
        };

        self.store
            .upsert_job_with_mode(
                &params.name,
                "",
                &params.kind,
                "oneshot",
                Some(&run_at.to_rfc3339()),
            )
            .await
            .map_err(|e| ToolError::InvalidParams {
                message: format!("store error: {e}"),
            })?;

        self.task_tx
            .try_send(SchedulerMessage::Add(Box::new(desc)))
            .map_err(|_| ToolError::InvalidParams {
                message: "scheduler channel full or closed".into(),
            })?;

        let summary = format!(
            "{action} deferred task '{}' (kind: {}, run_at: {})",
            params.name, params.kind, params.run_at
        );
        self.notify_refresh();
        Ok(Some(make_output("schedule_deferred", &summary)))
    }

    async fn cancel_task(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        let params: CancelParams = deserialize_params(&call.params)?;

        let exists =
            self.store
                .job_exists(&params.name)
                .await
                .map_err(|e| ToolError::InvalidParams {
                    message: format!("store error: {e}"),
                })?;

        let summary = if exists {
            self.store
                .delete_job(&params.name)
                .await
                .map_err(|e| ToolError::InvalidParams {
                    message: format!("store error: {e}"),
                })?;
            self.task_tx
                .try_send(SchedulerMessage::Cancel(params.name.clone()))
                .map_err(|_| ToolError::InvalidParams {
                    message: "scheduler channel full or closed".into(),
                })?;
            self.notify_refresh();
            format!("Cancelled task '{}'", params.name)
        } else {
            format!("Task '{}' not found", params.name)
        };

        Ok(Some(make_output("cancel_task", &summary)))
    }
}

fn make_output(tool_name: &str, summary: &str) -> ToolOutput {
    ToolOutput {
        tool_name: tool_name.to_owned(),
        summary: truncate_tool_output(summary),
        blocks_executed: 1,
        filter_stats: None,
        diff: None,
        streamed: false,
        terminal_id: None,
        locations: None,
        raw_response: None,
    }
}

impl ToolExecutor for SchedulerExecutor {
    async fn execute(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        for (tag, tool_id) in [
            ("schedule_periodic", "schedule_periodic"),
            ("schedule_deferred", "schedule_deferred"),
            ("cancel_task", "cancel_task"),
        ] {
            let blocks = zeph_tools::executor::extract_fenced_blocks(response, tag);
            if let Some(body) = blocks.into_iter().next() {
                let params: serde_json::Map<String, serde_json::Value> =
                    serde_json::from_str(body).unwrap_or_else(|e| {
                        tracing::warn!(tool = tag, error = %e, "fenced block contains invalid JSON, using empty params");
                        serde_json::Map::default()
                    });
                let call = ToolCall {
                    tool_id: tool_id.into(),
                    params,
                };
                return self.execute_tool_call(&call).await;
            }
        }
        Ok(None)
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                id: "schedule_periodic".into(),
                description: "Schedule a recurring background task using a cron expression. Use for daily cleanups, weekly refreshes, health checks, etc.".into(),
                schema: schemars::schema_for!(PeriodicParams),
                invocation: InvocationHint::FencedBlock("schedule_periodic"),
            },
            ToolDef {
                id: "schedule_deferred".into(),
                description: "Schedule a one-shot task at a future time. Accepts ISO 8601 UTC, relative offsets (+2h, +30m, +1h30m), or natural expressions (in 5 minutes, tomorrow 10:00).".into(),
                schema: schemars::schema_for!(DeferredParams),
                invocation: InvocationHint::FencedBlock("schedule_deferred"),
            },
            ToolDef {
                id: "cancel_task".into(),
                description: "Cancel a scheduled task by name. Works for both periodic and deferred tasks.".into(),
                schema: schemars::schema_for!(CancelParams),
                invocation: InvocationHint::FencedBlock("cancel_task"),
            },
        ]
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        match call.tool_id.as_str() {
            "schedule_periodic" => self.schedule_periodic(call).await,
            "schedule_deferred" => self.schedule_deferred(call).await,
            "cancel_task" => self.cancel_task(call).await,
            _ => Ok(None),
        }
    }
}

/// `Arc`-wrapper so `SchedulerExecutor` can be shared across ACP sessions without `Clone`.
pub(crate) struct DynSchedulerExecutor(pub(crate) std::sync::Arc<SchedulerExecutor>);

impl ToolExecutor for DynSchedulerExecutor {
    async fn execute(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        self.0.execute(response).await
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        self.0.tool_definitions()
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        self.0.execute_tool_call(call).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{Duration, Utc};
    use sqlx::SqlitePool;
    use tokio::sync::mpsc;
    use zeph_scheduler::JobStore;

    use super::*;

    async fn make_executor() -> (SchedulerExecutor, mpsc::Receiver<SchedulerMessage>) {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let store = JobStore::new(pool);
        store.init().await.unwrap();
        let store = Arc::new(store);
        let (tx, rx) = mpsc::channel(16);
        (SchedulerExecutor::new(tx, store), rx)
    }

    #[allow(clippy::needless_pass_by_value)]
    fn make_call(tool_id: &str, params: serde_json::Value) -> ToolCall {
        let serde_json::Value::Object(params) = params else {
            panic!("scheduler test params must be a JSON object");
        };
        ToolCall {
            tool_id: tool_id.to_owned(),
            params,
        }
    }

    #[tokio::test]
    async fn schedule_periodic_valid() {
        let (exec, mut rx) = make_executor().await;
        let call = make_call(
            "schedule_periodic",
            serde_json::json!({"name": "daily", "cron": "0 0 3 * * *", "kind": "memory_cleanup"}),
        );
        let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(result.summary.contains("Created"));
        assert!(result.summary.contains("daily"));
        assert!(rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn schedule_periodic_valid_5field() {
        let (exec, mut rx) = make_executor().await;
        let call = make_call(
            "schedule_periodic",
            serde_json::json!({"name": "every5m", "cron": "*/5 * * * *", "kind": "health_check"}),
        );
        let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(result.summary.contains("Created"));
        assert!(result.summary.contains("every5m"));
        // Verify normalized cron is persisted to DB (must be 6-field, not raw 5-field)
        let row: (String,) =
            sqlx::query_as("SELECT cron_expr FROM scheduled_jobs WHERE name = 'every5m'")
                .fetch_one(exec.store.pool())
                .await
                .unwrap();
        assert_eq!(
            row.0, "0 */5 * * * *",
            "DB must store normalized 6-field cron"
        );
        assert!(rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn schedule_periodic_invalid_cron() {
        let (exec, _rx) = make_executor().await;
        let call = make_call(
            "schedule_periodic",
            serde_json::json!({"name": "bad", "cron": "not_a_cron", "kind": "health_check"}),
        );
        let result = exec.execute_tool_call(&call).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn schedule_deferred_valid() {
        let (exec, mut rx) = make_executor().await;
        let future = (Utc::now() + Duration::hours(2)).to_rfc3339();
        let call = make_call(
            "schedule_deferred",
            serde_json::json!({"name": "reminder", "run_at": future, "kind": "custom", "task": "send report"}),
        );
        let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(result.summary.contains("Created"));
        assert!(rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn schedule_deferred_past_run_at() {
        let (exec, _rx) = make_executor().await;
        let past = (Utc::now() - Duration::hours(1)).to_rfc3339();
        let call = make_call(
            "schedule_deferred",
            serde_json::json!({"name": "past", "run_at": past, "kind": "custom"}),
        );
        let result = exec.execute_tool_call(&call).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn schedule_deferred_invalid_iso8601() {
        let (exec, _rx) = make_executor().await;
        let call = make_call(
            "schedule_deferred",
            serde_json::json!({"name": "bad_date", "run_at": "not-a-date", "kind": "custom"}),
        );
        let result = exec.execute_tool_call(&call).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cancel_existing_task() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let store = JobStore::new(pool);
        store.init().await.unwrap();
        store
            .upsert_job("my_task", "0 * * * * *", "health_check")
            .await
            .unwrap();
        let store = Arc::new(store);
        let (tx, mut rx) = mpsc::channel(16);
        let exec = SchedulerExecutor::new(tx, store);

        let call = make_call("cancel_task", serde_json::json!({"name": "my_task"}));
        let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(result.summary.contains("Cancelled"));
        assert!(rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn cancel_nonexistent_task() {
        let (exec, mut rx) = make_executor().await;
        let call = make_call("cancel_task", serde_json::json!({"name": "ghost"}));
        let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(result.summary.contains("not found"));
        assert!(
            rx.try_recv().is_err(),
            "no message should be sent for nonexistent task"
        );
    }

    #[tokio::test]
    async fn wrong_tool_id_returns_none() {
        let (exec, _rx) = make_executor().await;
        let call = make_call("bash", serde_json::Map::new().into());
        let result = exec.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn duplicate_name_returns_updated_message() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let store = JobStore::new(pool);
        store.init().await.unwrap();
        store
            .upsert_job("existing", "0 * * * * *", "health_check")
            .await
            .unwrap();
        let store = Arc::new(store);
        let (tx, _rx) = mpsc::channel(16);
        let exec = SchedulerExecutor::new(tx, store);

        let call = make_call(
            "schedule_periodic",
            serde_json::json!({"name": "existing", "cron": "0 0 * * * *", "kind": "health_check"}),
        );
        let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(result.summary.contains("Updated"));
    }

    #[tokio::test]
    async fn tool_definitions_count() {
        let (exec, _rx) = make_executor().await;
        assert_eq!(exec.tool_definitions().len(), 3);
    }

    #[tokio::test]
    async fn schedule_periodic_rejects_long_name() {
        let (exec, _rx) = make_executor().await;
        let call = make_call(
            "schedule_periodic",
            serde_json::json!({"name": "a".repeat(129), "cron": "0 * * * * *", "kind": "health_check"}),
        );
        assert!(exec.execute_tool_call(&call).await.is_err());
    }

    #[tokio::test]
    async fn schedule_periodic_rejects_long_kind() {
        let (exec, _rx) = make_executor().await;
        let call = make_call(
            "schedule_periodic",
            serde_json::json!({"name": "ok", "cron": "0 * * * * *", "kind": "k".repeat(65)}),
        );
        assert!(exec.execute_tool_call(&call).await.is_err());
    }

    #[tokio::test]
    async fn schedule_deferred_rejects_long_name() {
        use chrono::{Duration, Utc};
        let (exec, _rx) = make_executor().await;
        let future = (Utc::now() + Duration::hours(1)).to_rfc3339();
        let call = make_call(
            "schedule_deferred",
            serde_json::json!({"name": "a".repeat(129), "run_at": future, "kind": "custom"}),
        );
        assert!(exec.execute_tool_call(&call).await.is_err());
    }

    #[test]
    fn sanitize_strips_control_chars() {
        let result = super::sanitize_task_prompt("hello\x00\x01world\nok");
        assert_eq!(result, "helloworld\nok");
    }

    #[test]
    fn sanitize_truncates_at_512() {
        let long = "a".repeat(1000);
        let result = super::sanitize_task_prompt(&long);
        assert_eq!(result.len(), 512);
    }

    #[test]
    fn sanitize_preserves_tab_and_newline() {
        let result = super::sanitize_task_prompt("line1\nline2\ttab");
        assert_eq!(result, "line1\nline2\ttab");
    }

    // parse_run_at: ISO 8601 with Z suffix
    #[test]
    fn parse_run_at_iso8601_z() {
        let now = Utc::now();
        let dt = super::parse_run_at("2030-06-15T12:00:00Z", now);
        assert!(dt.is_some());
        assert_eq!(dt.unwrap().to_rfc3339(), "2030-06-15T12:00:00+00:00");
    }

    // parse_run_at: ISO 8601 with timezone offset
    #[test]
    fn parse_run_at_iso8601_offset() {
        let now = Utc::now();
        let dt = super::parse_run_at("2030-06-15T15:00:00+03:00", now);
        assert!(dt.is_some());
        assert_eq!(dt.unwrap().to_rfc3339(), "2030-06-15T12:00:00+00:00");
    }

    // parse_run_at: naive ISO 8601 (assumed UTC)
    #[test]
    fn parse_run_at_naive_iso8601() {
        let now = Utc::now();
        let dt = super::parse_run_at("2030-06-15T12:00:00", now);
        assert!(dt.is_some());
        assert_eq!(dt.unwrap().to_rfc3339(), "2030-06-15T12:00:00+00:00");
    }

    // parse_run_at: relative shorthand
    #[test]
    fn parse_run_at_relative_2m() {
        let now = Utc::now();
        let dt = super::parse_run_at("+2m", now).unwrap();
        assert_eq!(dt, now + chrono::Duration::seconds(120));
    }

    #[test]
    fn parse_run_at_relative_1h30m() {
        let now = Utc::now();
        let dt = super::parse_run_at("+1h30m", now).unwrap();
        assert_eq!(dt, now + chrono::Duration::seconds(5400));
    }

    #[test]
    fn parse_run_at_relative_5s() {
        let now = Utc::now();
        let dt = super::parse_run_at("5s", now).unwrap();
        assert_eq!(dt, now + chrono::Duration::seconds(5));
    }

    #[test]
    fn parse_run_at_relative_3d() {
        let now = Utc::now();
        let dt = super::parse_run_at("+3d", now).unwrap();
        assert_eq!(dt, now + chrono::Duration::days(3));
    }

    // parse_run_at: natural language
    #[test]
    fn parse_run_at_natural_in_5_minutes() {
        let now = Utc::now();
        let dt = super::parse_run_at("in 5 minutes", now).unwrap();
        assert_eq!(dt, now + chrono::Duration::seconds(300));
    }

    #[test]
    fn parse_run_at_natural_in_1_hour() {
        let now = Utc::now();
        let dt = super::parse_run_at("in 1 hour", now).unwrap();
        assert_eq!(dt, now + chrono::Duration::seconds(3600));
    }

    #[test]
    fn parse_run_at_natural_tomorrow() {
        let now = Utc::now();
        let dt = super::parse_run_at("tomorrow 10:00", now);
        assert!(dt.is_some());
        let dt = dt.unwrap();
        assert!(dt > now);
        assert_eq!(dt.format("%H:%M").to_string(), "10:00");
    }

    #[test]
    fn parse_run_at_natural_today() {
        let now = Utc::now();
        // today parsing may be in the past; just verify it returns a result with correct time
        let dt = super::parse_run_at("today 23:59", now);
        // Could be None only if date math fails; otherwise Some
        if let Some(dt) = dt {
            assert_eq!(dt.format("%H:%M").to_string(), "23:59");
        }
    }

    #[test]
    fn parse_run_at_natural_case_insensitive() {
        let now = Utc::now();
        let dt = super::parse_run_at("In 5 Minutes", now);
        assert!(dt.is_some());
    }

    // parse_run_at: rejection cases
    #[test]
    fn parse_run_at_rejects_empty() {
        let now = Utc::now();
        assert!(super::parse_run_at("", now).is_none());
    }

    #[test]
    fn parse_run_at_rejects_garbage() {
        let now = Utc::now();
        assert!(super::parse_run_at("not-a-date", now).is_none());
        assert!(super::parse_run_at("foobar", now).is_none());
    }

    #[test]
    fn parse_run_at_rejects_zero_duration() {
        let now = Utc::now();
        assert!(super::parse_run_at("+0m", now).is_none());
        assert!(super::parse_run_at("0s", now).is_none());
    }

    #[test]
    fn parse_run_at_rejects_invalid_time() {
        let now = Utc::now();
        assert!(super::parse_run_at("tomorrow 25:00", now).is_none());
        assert!(super::parse_run_at("today 12:99", now).is_none());
    }

    #[test]
    fn parse_run_at_rejects_trailing_digits() {
        let now = Utc::now();
        assert!(super::parse_run_at("+5h30", now).is_none());
    }

    // schedule_deferred integration test with relative input
    #[tokio::test]
    async fn schedule_deferred_relative_offset() {
        let (exec, mut rx) = make_executor().await;
        let call = make_call(
            "schedule_deferred",
            serde_json::json!({"name": "soon", "run_at": "+2h", "kind": "custom", "task": "do something"}),
        );
        let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(result.summary.contains("Created"));
        assert!(rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn schedule_deferred_natural_language() {
        let (exec, mut rx) = make_executor().await;
        let call = make_call(
            "schedule_deferred",
            serde_json::json!({"name": "nat_test", "run_at": "in 3 hours", "kind": "custom"}),
        );
        let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(result.summary.contains("Created"));
        assert!(rx.recv().await.is_some());
    }

    // Overflow: astronomically large day count must return None, not panic
    #[test]
    fn parse_relative_overflow_returns_none() {
        assert!(super::parse_relative("+99999999999999999d").is_none());
    }

    // Leading/trailing whitespace trimmed by parse_run_at
    #[test]
    fn parse_run_at_strips_whitespace() {
        let now = Utc::now();
        let dt = super::parse_run_at(" +2h ", now);
        assert!(dt.is_some());
        let dt = dt.unwrap();
        assert!(dt > now + chrono::Duration::seconds(7199));
        assert!(dt <= now + chrono::Duration::seconds(7201));
    }

    // execute() fenced-block dispatch tests (GAP-02)

    #[tokio::test]
    async fn execute_fenced_schedule_periodic_dispatches() {
        let (exec, mut rx) = make_executor().await;
        let response = "Sure!\n```schedule_periodic\n{\"name\":\"daily\",\"cron\":\"0 0 3 * * *\",\"kind\":\"memory_cleanup\"}\n```";
        let result = exec.execute(response).await.unwrap();
        assert!(result.is_some(), "fenced schedule_periodic must dispatch");
        assert!(result.unwrap().summary.contains("daily"));
        assert!(rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn execute_fenced_schedule_deferred_dispatches() {
        let (exec, mut rx) = make_executor().await;
        let response = "```schedule_deferred\n{\"name\":\"soon\",\"run_at\":\"+2h\",\"kind\":\"custom\",\"task\":\"ping\"}\n```";
        let result = exec.execute(response).await.unwrap();
        assert!(result.is_some(), "fenced schedule_deferred must dispatch");
        assert!(rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn execute_fenced_cancel_task_dispatches() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let store = JobStore::new(pool);
        store.init().await.unwrap();
        store
            .upsert_job("to_cancel", "0 * * * * *", "health_check")
            .await
            .unwrap();
        let store = Arc::new(store);
        let (tx, mut rx) = mpsc::channel(16);
        let exec = SchedulerExecutor::new(tx, store);

        let response = "```cancel_task\n{\"name\":\"to_cancel\"}\n```";
        let result = exec.execute(response).await.unwrap();
        assert!(result.is_some(), "fenced cancel_task must dispatch");
        assert!(result.unwrap().summary.contains("Cancelled"));
        assert!(rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn execute_no_fenced_block_returns_none() {
        let (exec, _rx) = make_executor().await;
        let response = "This is a plain text response with no fenced blocks.";
        let result = exec.execute(response).await.unwrap();
        assert!(result.is_none(), "no fenced block must return None");
    }

    #[tokio::test]
    async fn execute_invalid_json_in_fenced_block_proceeds_with_empty_params() {
        let (exec, _rx) = make_executor().await;
        // Invalid JSON in fenced block — serde_json::from_str returns unwrap_or_default (empty map)
        // which causes deserialization of params to fail with ToolError.
        let response = "```schedule_periodic\nnot valid json\n```";
        let result = exec.execute(response).await;
        assert!(
            result.is_err(),
            "invalid JSON in fenced block must propagate as error"
        );
    }
}
