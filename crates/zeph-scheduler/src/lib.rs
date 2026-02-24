// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

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
