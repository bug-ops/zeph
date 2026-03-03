// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::str::FromStr;
use std::sync::Arc;

use chrono::Utc;
use cron::Schedule as CronSchedule;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::mpsc;
use zeph_scheduler::{
    JobStore, SchedulerMessage, TaskDescriptor, TaskKind, TaskMode, sanitize_task_prompt,
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
    pub run_at: String,
    pub kind: String,
    #[serde(default)]
    pub task: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CancelParams {
    pub name: String,
}

/// Tool executor that exposes scheduler management to the LLM.
pub struct SchedulerExecutor {
    task_tx: mpsc::Sender<SchedulerMessage>,
    store: Arc<JobStore>,
}

impl SchedulerExecutor {
    #[must_use]
    pub fn new(task_tx: mpsc::Sender<SchedulerMessage>, store: Arc<JobStore>) -> Self {
        Self { task_tx, store }
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

        let schedule =
            CronSchedule::from_str(&params.cron).map_err(|e| ToolError::InvalidParams {
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

        self.task_tx
            .try_send(SchedulerMessage::Add(Box::new(desc)))
            .map_err(|_| ToolError::InvalidParams {
                message: "scheduler channel full or closed".into(),
            })?;

        let summary = format!(
            "{action} periodic task '{}' (kind: {}, next run: {next_run})",
            params.name, params.kind
        );
        Ok(Some(make_output("schedule_periodic", &summary)))
    }

    async fn schedule_deferred(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        let params: DeferredParams = deserialize_params(&call.params)?;

        if params.name.len() > 128 {
            return Err(ToolError::InvalidParams {
                message: "name exceeds 128 characters".into(),
            });
        }

        if params.kind.len() > 64 {
            return Err(ToolError::InvalidParams {
                message: "kind exceeds 64 characters".into(),
            });
        }

        let run_at = params
            .run_at
            .parse::<chrono::DateTime<Utc>>()
            .map_err(|_| ToolError::InvalidParams {
                message: "run_at must be ISO 8601 UTC, e.g. 2026-03-03T18:00:00Z".into(),
            })?;

        if run_at <= Utc::now() {
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

        self.task_tx
            .try_send(SchedulerMessage::Add(Box::new(desc)))
            .map_err(|_| ToolError::InvalidParams {
                message: "scheduler channel full or closed".into(),
            })?;

        let summary = format!(
            "{action} deferred task '{}' (kind: {}, run_at: {})",
            params.name, params.kind, params.run_at
        );
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
            self.task_tx
                .try_send(SchedulerMessage::Cancel(params.name.clone()))
                .map_err(|_| ToolError::InvalidParams {
                    message: "scheduler channel full or closed".into(),
                })?;
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
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                id: "schedule_periodic".into(),
                description: "Schedule a recurring background task using a cron expression. Use for daily cleanups, weekly refreshes, health checks, etc.".into(),
                schema: schemars::schema_for!(PeriodicParams),
                invocation: InvocationHint::ToolCall,
            },
            ToolDef {
                id: "schedule_deferred".into(),
                description: "Schedule a one-shot task to run at a specific future time (ISO 8601 UTC). Use for reminders, follow-ups, or time-specific actions.".into(),
                schema: schemars::schema_for!(DeferredParams),
                invocation: InvocationHint::ToolCall,
            },
            ToolDef {
                id: "cancel_task".into(),
                description: "Cancel a scheduled task by name. Works for both periodic and deferred tasks.".into(),
                schema: schemars::schema_for!(CancelParams),
                invocation: InvocationHint::ToolCall,
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

    fn make_call(tool_id: &str, params: serde_json::Value) -> ToolCall {
        ToolCall {
            tool_id: tool_id.to_owned(),
            params: params.as_object().unwrap().clone(),
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
}
