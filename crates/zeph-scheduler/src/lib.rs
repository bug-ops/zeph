// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Cron-based periodic task scheduler with `SQLite` persistence.
//!
//! `zeph-scheduler` drives time-based work inside the Zeph agent. It supports two
//! task execution modes:
//!
//! - **Periodic** — tasks defined by a cron expression and re-scheduled after each run.
//! - **One-shot** — tasks that run once at a specific `DateTime<Utc>` and are removed
//!   on completion.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────┐
//! │              Scheduler               │
//! │  tasks: Vec<ScheduledTask>           │
//! │  handlers: HashMap<kind, TaskHandler>│
//! │  store: JobStore (SQLite)            │
//! │  shutdown_rx: watch::Receiver<bool>  │
//! │  task_rx: mpsc::Receiver<Msg>        │
//! └───────────┬──────────────────────────┘
//!             │ tick() every N seconds
//!             ▼
//!     for each due task → handler.execute()
//!             │
//!             ▼
//!     store.record_run() / store.mark_done()
//! ```
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use tokio::sync::watch;
//! use zeph_scheduler::{JobStore, Scheduler, ScheduledTask, TaskKind};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // 1. Open the job store.
//! let store = JobStore::open("sqlite:scheduler.db").await?;
//!
//! // 2. Create the scheduler.
//! let (_shutdown_tx, shutdown_rx) = watch::channel(false);
//! let (mut scheduler, _msg_tx) = Scheduler::new(store, shutdown_rx);
//!
//! // 3. Register a periodic task (every minute).
//! let task = ScheduledTask::new(
//!     "health-check",
//!     "*/1 * * * *",
//!     TaskKind::HealthCheck,
//!     serde_json::Value::Null,
//! )?;
//! scheduler.add_task(task);
//!
//! // 4. Initialise persistence and run.
//! scheduler.init().await?;
//! scheduler.run().await;
//! # Ok(())
//! # }
//! ```
//!
//! # Cron Expression Format
//!
//! The scheduler accepts both **5-field** (`min hour day month weekday`) and
//! **6-field** (`sec min hour day month weekday`) cron expressions. Five-field
//! expressions are automatically normalised by prepending `"0 "` (seconds = 0).
//! Use [`normalize_cron_expr`] directly if you need the canonical form.
//!
//! # Shutdown
//!
//! Send `true` on the `watch::Sender<bool>` that was passed to [`Scheduler::new`]
//! to trigger a graceful shutdown. The scheduler loop exits on the next tick.
//!
//! # `SQLite` Persistence
//!
//! All job definitions and run history are stored in a `SQLite` database managed by
//! `zeph-db` migrations. Use [`JobStore::open`] to connect or [`JobStore::new`] when
//! you already hold a [`zeph_db::DbPool`].

#[allow(unused_imports)]
pub(crate) use zeph_db::sql;

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
