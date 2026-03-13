// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Cron-based periodic task scheduler with `SQLite` persistence.

mod error;
mod handlers;
mod sanitize;
mod scheduler;
mod store;
mod task;
pub mod update_check;

pub use error::SchedulerError;
pub use handlers::CustomTaskHandler;
pub use sanitize::sanitize_task_prompt;
pub use scheduler::{Scheduler, SchedulerMessage};
pub use store::{JobStore, ScheduledTaskInfo};
pub use task::{
    ScheduledTask, TaskDescriptor, TaskHandler, TaskKind, TaskMode, normalize_cron_expr,
};
pub use update_check::UpdateCheckHandler;
