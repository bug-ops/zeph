//! Cron-based periodic task scheduler with `SQLite` persistence.

mod error;
mod scheduler;
mod store;
mod task;
pub mod update_check;

pub use error::SchedulerError;
pub use scheduler::Scheduler;
pub use store::JobStore;
pub use task::{ScheduledTask, TaskHandler, TaskKind};
pub use update_check::UpdateCheckHandler;
