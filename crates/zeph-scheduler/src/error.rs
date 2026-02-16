use thiserror::Error;

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("invalid cron expression: {0}")]
    InvalidCron(String),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("task execution failed: {0}")]
    TaskFailed(String),
}
