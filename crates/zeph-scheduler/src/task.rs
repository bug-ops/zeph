// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::borrow::Cow;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use cron::Schedule as CronSchedule;

use crate::error::SchedulerError;

/// Normalize a cron expression to the 6-field format required by the `cron` crate.
///
/// Standard 5-field expressions (`min hour day month weekday`) are prepended with `"0 "` to
/// default seconds to zero. 6-field expressions are passed through unchanged. Any other field
/// count is also passed through and will produce an error from the `cron` crate at parse time.
#[must_use]
pub fn normalize_cron_expr(expr: &str) -> Cow<'_, str> {
    if expr.split_whitespace().count() == 5 {
        Cow::Owned(format!("0 {expr}"))
    } else {
        Cow::Borrowed(expr)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskKind {
    MemoryCleanup,
    SkillRefresh,
    HealthCheck,
    UpdateCheck,
    Experiment,
    Custom(String),
}

impl TaskKind {
    #[must_use]
    pub fn from_str_kind(s: &str) -> Self {
        match s {
            "memory_cleanup" => Self::MemoryCleanup,
            "skill_refresh" => Self::SkillRefresh,
            "health_check" => Self::HealthCheck,
            "update_check" => Self::UpdateCheck,
            "experiment" => Self::Experiment,
            other => Self::Custom(other.to_owned()),
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::MemoryCleanup => "memory_cleanup",
            Self::SkillRefresh => "skill_refresh",
            Self::HealthCheck => "health_check",
            Self::UpdateCheck => "update_check",
            Self::Experiment => "experiment",
            Self::Custom(s) => s,
        }
    }
}

/// Execution mode for a scheduled task.
pub enum TaskMode {
    Periodic { schedule: Box<CronSchedule> },
    OneShot { run_at: DateTime<Utc> },
}

/// Descriptor sent over the mpsc channel to register tasks at runtime.
pub struct TaskDescriptor {
    pub name: String,
    pub mode: TaskMode,
    pub kind: TaskKind,
    pub config: serde_json::Value,
}

pub struct ScheduledTask {
    pub name: String,
    pub mode: TaskMode,
    pub kind: TaskKind,
    pub config: serde_json::Value,
}

impl ScheduledTask {
    /// Create a new periodic task from a cron expression string.
    ///
    /// # Errors
    ///
    /// Returns `SchedulerError::InvalidCron` if the expression is not valid.
    pub fn new(
        name: impl Into<String>,
        cron_expr: &str,
        kind: TaskKind,
        config: serde_json::Value,
    ) -> Result<Self, SchedulerError> {
        Self::periodic(name, cron_expr, kind, config)
    }

    /// Create a periodic task from a cron expression.
    ///
    /// # Errors
    ///
    /// Returns `SchedulerError::InvalidCron` if the expression is not valid.
    pub fn periodic(
        name: impl Into<String>,
        cron_expr: &str,
        kind: TaskKind,
        config: serde_json::Value,
    ) -> Result<Self, SchedulerError> {
        let normalized = normalize_cron_expr(cron_expr);
        let schedule = CronSchedule::from_str(&normalized)
            .map_err(|e| SchedulerError::InvalidCron(format!("{cron_expr}: {e}")))?;
        Ok(Self {
            name: name.into(),
            mode: TaskMode::Periodic {
                schedule: Box::new(schedule),
            },
            kind,
            config,
        })
    }

    /// Create a one-shot task that runs at a specific point in time.
    #[must_use]
    pub fn oneshot(
        name: impl Into<String>,
        run_at: DateTime<Utc>,
        kind: TaskKind,
        config: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            mode: TaskMode::OneShot { run_at },
            kind,
            config,
        }
    }

    /// Returns the cron schedule if this is a periodic task.
    #[must_use]
    pub fn cron_schedule(&self) -> Option<&CronSchedule> {
        if let TaskMode::Periodic { schedule } = &self.mode {
            Some(schedule.as_ref())
        } else {
            None
        }
    }

    /// Returns the cron expression string for DB persistence (periodic tasks only).
    #[must_use]
    pub fn cron_expr_string(&self) -> String {
        match &self.mode {
            TaskMode::Periodic { schedule } => schedule.to_string(),
            TaskMode::OneShot { .. } => String::new(),
        }
    }

    /// Returns `task_mode` string for DB persistence.
    #[must_use]
    pub fn task_mode_str(&self) -> &'static str {
        match &self.mode {
            TaskMode::Periodic { .. } => "periodic",
            TaskMode::OneShot { .. } => "oneshot",
        }
    }
}

pub trait TaskHandler: Send + Sync {
    fn execute(
        &self,
        config: &serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), SchedulerError>> + Send + '_>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_kind_roundtrip() {
        assert_eq!(
            TaskKind::from_str_kind("memory_cleanup"),
            TaskKind::MemoryCleanup
        );
        assert_eq!(TaskKind::MemoryCleanup.as_str(), "memory_cleanup");
        assert_eq!(
            TaskKind::from_str_kind("skill_refresh"),
            TaskKind::SkillRefresh
        );
        assert_eq!(TaskKind::SkillRefresh.as_str(), "skill_refresh");
        assert_eq!(
            TaskKind::from_str_kind("health_check"),
            TaskKind::HealthCheck
        );
        assert_eq!(
            TaskKind::from_str_kind("update_check"),
            TaskKind::UpdateCheck
        );
        assert_eq!(TaskKind::UpdateCheck.as_str(), "update_check");
        assert_eq!(
            TaskKind::from_str_kind("custom_job"),
            TaskKind::Custom("custom_job".into())
        );
        assert_eq!(TaskKind::Custom("x".into()).as_str(), "x");
    }

    #[test]
    fn task_kind_experiment_roundtrip() {
        assert_eq!(
            TaskKind::from_str_kind("experiment"),
            TaskKind::Experiment,
            "from_str_kind must map 'experiment' to Experiment variant, not Custom"
        );
        assert_eq!(TaskKind::Experiment.as_str(), "experiment");
    }

    #[test]
    fn normalize_five_field_prepends_zero() {
        assert_eq!(normalize_cron_expr("*/5 * * * *"), "0 */5 * * * *");
        assert_eq!(normalize_cron_expr("0 3 * * *"), "0 0 3 * * *");
    }

    #[test]
    fn normalize_six_field_passthrough() {
        assert_eq!(normalize_cron_expr("0 0 3 * * *"), "0 0 3 * * *");
        assert_eq!(normalize_cron_expr("* * * * * *"), "* * * * * *");
    }

    #[test]
    fn normalize_other_field_count_passthrough() {
        assert_eq!(normalize_cron_expr("not_cron"), "not_cron");
        assert_eq!(normalize_cron_expr("0 0 0 0"), "0 0 0 0");
    }

    #[test]
    fn normalize_empty_string_passthrough() {
        assert_eq!(normalize_cron_expr(""), "");
    }

    #[test]
    fn normalize_whitespace_only_passthrough() {
        assert_eq!(normalize_cron_expr("   "), "   ");
    }

    #[test]
    fn valid_cron_creates_task() {
        let task = ScheduledTask::new(
            "test",
            "0 0 * * * *",
            TaskKind::HealthCheck,
            serde_json::Value::Null,
        );
        assert!(task.is_ok());
    }

    #[test]
    fn five_field_cron_creates_task() {
        let task = ScheduledTask::new(
            "five-field",
            "*/5 * * * *",
            TaskKind::HealthCheck,
            serde_json::Value::Null,
        );
        assert!(task.is_ok(), "5-field cron must be accepted");
    }

    #[test]
    fn invalid_cron_returns_error() {
        let task = ScheduledTask::new(
            "test",
            "not_cron",
            TaskKind::HealthCheck,
            serde_json::Value::Null,
        );
        assert!(task.is_err());
    }

    #[test]
    fn oneshot_task_creates_correctly() {
        let run_at = Utc::now() + chrono::Duration::hours(1);
        let task =
            ScheduledTask::oneshot("t", run_at, TaskKind::HealthCheck, serde_json::Value::Null);
        assert_eq!(task.task_mode_str(), "oneshot");
        assert!(task.cron_schedule().is_none());
    }

    #[test]
    fn periodic_task_mode_str() {
        let task = ScheduledTask::periodic(
            "p",
            "0 * * * * *",
            TaskKind::HealthCheck,
            serde_json::Value::Null,
        )
        .unwrap();
        assert_eq!(task.task_mode_str(), "periodic");
        assert!(task.cron_schedule().is_some());
    }
}
