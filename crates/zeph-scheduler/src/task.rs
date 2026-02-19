use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;

use cron::Schedule as CronSchedule;

use crate::error::SchedulerError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskKind {
    MemoryCleanup,
    SkillRefresh,
    HealthCheck,
    UpdateCheck,
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
            Self::Custom(s) => s,
        }
    }
}

pub struct ScheduledTask {
    pub name: String,
    pub schedule: CronSchedule,
    pub kind: TaskKind,
    pub config: serde_json::Value,
}

impl ScheduledTask {
    /// Create a new scheduled task from a cron expression string.
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
        let schedule = CronSchedule::from_str(cron_expr)
            .map_err(|e| SchedulerError::InvalidCron(format!("{cron_expr}: {e}")))?;
        Ok(Self {
            name: name.into(),
            schedule,
            kind,
            config,
        })
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
    fn invalid_cron_returns_error() {
        let task = ScheduledTask::new(
            "test",
            "not_cron",
            TaskKind::HealthCheck,
            serde_json::Value::Null,
        );
        assert!(task.is_err());
    }
}
