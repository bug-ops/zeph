// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::borrow::Cow;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use cron::Schedule as CronSchedule;

use crate::error::SchedulerError;

/// Normalise a cron expression to the 6-field format required by the `cron` crate.
///
/// Standard 5-field expressions (`min hour day month weekday`) are prepended with `"0 "` to
/// default seconds to zero. 6-field expressions are passed through unchanged. Any other field
/// count is also passed through unchanged and will produce an error from the `cron` crate at
/// parse time.
///
/// # Examples
///
/// ```
/// use zeph_scheduler::normalize_cron_expr;
///
/// // 5-field: seconds are defaulted to 0.
/// assert_eq!(normalize_cron_expr("*/5 * * * *").as_ref(), "0 */5 * * * *");
///
/// // 6-field: passed through unchanged.
/// assert_eq!(normalize_cron_expr("0 */5 * * * *").as_ref(), "0 */5 * * * *");
/// ```
#[must_use]
pub fn normalize_cron_expr(expr: &str) -> Cow<'_, str> {
    if expr.split_whitespace().count() == 5 {
        Cow::Owned(format!("0 {expr}"))
    } else {
        Cow::Borrowed(expr)
    }
}

/// Identifies what type of work a scheduled task performs.
///
/// Built-in variants map to well-known agent subsystems. [`TaskKind::Custom`]
/// carries an arbitrary string so callers can define their own task kinds without
/// modifying this enum.
///
/// # Persistence
///
/// Each variant serialises to a stable `snake_case` string via [`TaskKind::as_str`]
/// and deserialises via [`TaskKind::from_str_kind`]. These strings are stored in
/// the `kind` column of the `scheduled_jobs` table.
///
/// # Examples
///
/// ```
/// use zeph_scheduler::TaskKind;
///
/// assert_eq!(TaskKind::HealthCheck.as_str(), "health_check");
/// assert_eq!(TaskKind::from_str_kind("memory_cleanup"), TaskKind::MemoryCleanup);
/// assert_eq!(TaskKind::from_str_kind("my_custom"), TaskKind::Custom("my_custom".into()));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskKind {
    /// Triggers the memory subsystem's cleanup / compaction routine.
    MemoryCleanup,
    /// Reloads skills from the skill registry.
    SkillRefresh,
    /// Runs a liveness or readiness probe for the agent.
    HealthCheck,
    /// Checks the GitHub releases API for a newer Zeph version.
    UpdateCheck,
    /// Runs an experiment task (used by `zeph-experiments`).
    Experiment,
    /// An application-defined task kind. The string is the persistence key.
    Custom(String),
}

impl TaskKind {
    /// Parse a task kind from its persistence string.
    ///
    /// Unknown strings are wrapped in [`TaskKind::Custom`] rather than returning
    /// an error, so new built-in variants added in future versions do not break
    /// existing stored jobs loaded with an older build.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_scheduler::TaskKind;
    ///
    /// assert_eq!(TaskKind::from_str_kind("health_check"), TaskKind::HealthCheck);
    /// assert_eq!(TaskKind::from_str_kind("unknown"), TaskKind::Custom("unknown".into()));
    /// ```
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

    /// Return the stable string key used for database persistence.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_scheduler::TaskKind;
    ///
    /// assert_eq!(TaskKind::SkillRefresh.as_str(), "skill_refresh");
    /// assert_eq!(TaskKind::Custom("my_job".into()).as_str(), "my_job");
    /// ```
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
///
/// Determines how the scheduler decides when to run a task and what to do after it
/// completes:
///
/// - [`TaskMode::Periodic`] re-computes `next_run` from the cron schedule after
///   each successful execution and never removes the task from memory.
/// - [`TaskMode::OneShot`] fires once when `now >= run_at` and then removes the
///   task from the in-memory task list and marks it `done` in the store.
pub enum TaskMode {
    /// Run on a repeating cron schedule.
    Periodic {
        /// Parsed cron schedule that drives `next_run` computation.
        schedule: Box<CronSchedule>,
    },
    /// Run once at the specified UTC timestamp.
    OneShot {
        /// The earliest UTC time at which the task should execute.
        run_at: DateTime<Utc>,
    },
}

/// Descriptor sent over the control channel to register tasks at runtime.
///
/// Send a `SchedulerMessage::Add` wrapping a boxed `TaskDescriptor` to add a
/// new task (or replace an existing one with the same name) without stopping the
/// scheduler loop.
pub struct TaskDescriptor {
    /// Unique name for the task. Replaces any existing task with the same name.
    pub name: String,
    /// Execution mode (periodic or one-shot).
    pub mode: TaskMode,
    /// The category of work this task performs.
    pub kind: TaskKind,
    /// Arbitrary JSON configuration forwarded to the [`TaskHandler`] at execution time.
    pub config: serde_json::Value,
}

/// A task held in memory by the [`crate::Scheduler`].
///
/// Use [`ScheduledTask::new`] / [`ScheduledTask::periodic`] for cron-based tasks
/// and [`ScheduledTask::oneshot`] for tasks that run at a fixed point in time.
///
/// # Examples
///
/// ```
/// use zeph_scheduler::{ScheduledTask, TaskKind};
///
/// let task = ScheduledTask::new(
///     "daily-cleanup",
///     "0 3 * * *",           // every day at 03:00 UTC (5-field cron)
///     TaskKind::MemoryCleanup,
///     serde_json::Value::Null,
/// )
/// .expect("valid cron expression");
///
/// assert_eq!(task.task_mode_str(), "periodic");
/// assert!(task.cron_schedule().is_some());
/// ```
pub struct ScheduledTask {
    /// Unique task name used as the primary key in the job store.
    pub name: String,
    /// Execution mode (periodic or one-shot).
    pub mode: TaskMode,
    /// The category of work this task performs.
    pub kind: TaskKind,
    /// Arbitrary JSON configuration forwarded to the [`TaskHandler`] at execution time.
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

    /// Returns the canonical 6-field cron expression string for DB persistence.
    ///
    /// Returns an empty string for one-shot tasks, which do not have a cron schedule.
    #[must_use]
    pub fn cron_expr_string(&self) -> String {
        match &self.mode {
            TaskMode::Periodic { schedule } => schedule.to_string(),
            TaskMode::OneShot { .. } => String::new(),
        }
    }

    /// Returns the `task_mode` string used for DB persistence.
    ///
    /// Returns `"periodic"` or `"oneshot"`.
    #[must_use]
    pub fn task_mode_str(&self) -> &'static str {
        match &self.mode {
            TaskMode::Periodic { .. } => "periodic",
            TaskMode::OneShot { .. } => "oneshot",
        }
    }
}

/// Trait for types that can execute a scheduled task.
///
/// Implementations receive the per-task JSON configuration stored in
/// [`ScheduledTask::config`] and return `Ok(())` on success or a
/// [`SchedulerError`] on failure. Failures are logged as warnings; the scheduler
/// continues running and will retry on the next due tick.
///
/// Because async trait methods in Edition 2024 require returning a pinned boxed
/// future for object safety, implementations must wrap their async work in
/// `Box::pin(async move { … })`.
///
/// # Example
///
/// ```rust
/// use std::future::Future;
/// use std::pin::Pin;
/// use zeph_scheduler::{SchedulerError, TaskHandler};
///
/// struct NoopHandler;
///
/// impl TaskHandler for NoopHandler {
///     fn execute(
///         &self,
///         _config: &serde_json::Value,
///     ) -> Pin<Box<dyn Future<Output = Result<(), SchedulerError>> + Send + '_>> {
///         Box::pin(async move { Ok(()) })
///     }
/// }
/// ```
pub trait TaskHandler: Send + Sync {
    /// Execute the task with the provided configuration.
    ///
    /// # Errors
    ///
    /// Return [`SchedulerError::TaskFailed`] (or any other variant) to indicate
    /// that the task could not complete successfully. The error is logged but does
    /// not stop the scheduler.
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
