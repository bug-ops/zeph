# zeph-scheduler

[![Crates.io](https://img.shields.io/crates/v/zeph-scheduler)](https://crates.io/crates/zeph-scheduler)
[![docs.rs](https://img.shields.io/docsrs/zeph-scheduler)](https://docs.rs/zeph-scheduler)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)

Cron-based periodic and one-shot task scheduler with SQLite persistence for Zeph.

## Overview

Manages recurring and deferred background tasks. Periodic tasks run on a cron schedule; one-shot tasks fire at a specific point in time. All job state, next-run timestamps, and task mode are persisted in SQLite. The scheduler is controlled at runtime via an `mpsc` channel — tasks can be added or cancelled without restarting the agent. When combined with the `experiments` feature flag, the scheduler can run autonomous experiment sessions on a cron schedule via `TaskKind::Experiment`. Feature-gated behind `scheduler`.

## Key Modules

- **scheduler** — `Scheduler` event loop; evaluates due tasks on each tick, drains the `SchedulerMessage` channel, and dispatches execution to registered handlers
- **store** — `JobStore` for SQLite-backed job persistence (upsert, record_run, mark_done, delete, next_run management)
- **task** — `ScheduledTask`, `TaskDescriptor`, `TaskHandler`, `TaskKind`, `TaskMode` — core type definitions
- **handlers** — `CustomTaskHandler` — injects a sanitized prompt into the agent loop via `mpsc::Sender<String>`
- **sanitize** — `sanitize_task_prompt` — strips control characters and truncates to 512 code points
- **update_check** — `UpdateCheckHandler` for GitHub releases version check
- **error** — `SchedulerError` error types

## Task Modes

`TaskMode` controls when a task fires:

| Variant | Trigger | Persistence |
|---------|---------|-------------|
| `TaskMode::Periodic { schedule }` | 5 or 6-field cron expression; fires every matching occurrence | `cron_expr` + `next_run` columns |
| `TaskMode::OneShot { run_at }` | Single ISO 8601 UTC timestamp | `run_at` column; removed from memory after execution |

> [!NOTE]
> One-shot tasks are automatically removed from the in-memory task list and marked `done` in the store after they execute. Upsert an existing name to update a task in place.

## Runtime Control via SchedulerMessage

The `Scheduler` exposes an `mpsc::Sender<SchedulerMessage>` returned from `Scheduler::new()`. The LLM (via `SchedulerExecutor`) and other subsystems send messages on this channel to add or cancel tasks without touching the scheduler loop directly.

```rust
pub enum SchedulerMessage {
    Add(Box<TaskDescriptor>),
    Cancel(String),   // task name
}
```

Messages are drained at the start of every tick. The channel capacity is 64 slots; `try_send` is used to avoid blocking.

## Built-in Tasks

| Kind | String key | Description |
|------|-----------|-------------|
| `TaskKind::MemoryCleanup` | `memory_cleanup` | Prune expired memory entries |
| `TaskKind::SkillRefresh` | `skill_refresh` | Hot-reload changed skill files |
| `TaskKind::HealthCheck` | `health_check` | Periodic self-diagnostics |
| `TaskKind::UpdateCheck` | `update_check` | Check GitHub releases for a newer version |
| `TaskKind::Experiment` | `experiment` | Run an autonomous experiment session (requires `experiments` feature) |
| `TaskKind::Custom(String)` | any other string | Custom prompt injected into the agent loop |

## CustomTaskHandler

`CustomTaskHandler` implements `TaskHandler` and forwards the `task` field from the job config as a sanitized prompt string to the agent loop via `mpsc::Sender<String>`. It is safe to call when the channel is full or closed — both conditions log a warning and return `Ok(())`.

```rust
use tokio::sync::mpsc;
use zeph_scheduler::{CustomTaskHandler, Scheduler, ScheduledTask, TaskKind};

let (agent_tx, agent_rx) = mpsc::channel(16);
let handler = CustomTaskHandler::new(agent_tx);
scheduler.register_handler(&TaskKind::Custom("my_task".into()), Box::new(handler));
```

## sanitize_task_prompt

User-supplied task prompts pass through `sanitize_task_prompt` before being injected into the agent loop. The function strips control characters below U+0020 (except `\n` and `\t`) and truncates to 512 Unicode code points.

```rust
use zeph_scheduler::sanitize_task_prompt;

let safe = sanitize_task_prompt("hello\x00\x01world\nok");
assert_eq!(safe, "helloworld\nok");
```

## UpdateCheckHandler

`UpdateCheckHandler` implements `TaskHandler` and queries the GitHub releases API to compare the running version against the latest published release. When a newer version is detected it sends a human-readable notification over an `mpsc::Sender<String>` channel.

```rust
use tokio::sync::mpsc;
use zeph_scheduler::{ScheduledTask, Scheduler, TaskKind, UpdateCheckHandler};

let (tx, rx) = mpsc::channel(4);
let handler = UpdateCheckHandler::new(env!("CARGO_PKG_VERSION"), tx);

let task = ScheduledTask::new(
    "update_check",
    "0 0 9 * * *",   // daily at 09:00
    TaskKind::UpdateCheck,
    serde_json::Value::Null,
)?;
scheduler.add_task(task);
scheduler.register_handler(&TaskKind::UpdateCheck, Box::new(handler));
```

Notification format sent via the channel:

```
New version available: v0.13.0 (current: v0.12.0).
Update: https://github.com/bug-ops/zeph/releases/tag/v0.12.0
```

Behaviour on error (network failure, non-2xx response, oversized body, parse error, invalid semver) — logs a `warn` message and returns `Ok(())`.

## Configuration

| Config field | Type | Default | Description |
|---|---|---|---|
| `tick_interval_secs` | u64 | `60` | How often the scheduler wakes to evaluate due tasks (minimum 1 second, enforced by `run_with_interval`) |
| `max_tasks` | usize | `100` | Maximum number of tasks held in memory; new tasks beyond this limit are dropped with a `warn` log |

Use `Scheduler::with_max_tasks(store, shutdown_rx, max)` to set the limit at construction time. Pass `tick_interval_secs` to `run_with_interval()`:

```rust
use tokio::sync::watch;
use zeph_scheduler::{JobStore, Scheduler};

let store = JobStore::open("scheduler.db").await?;
let (_, shutdown_rx) = watch::channel(false);
let (mut scheduler, task_tx) = Scheduler::with_max_tasks(store, shutdown_rx, 200);
scheduler.init().await?;
scheduler.run_with_interval(30).await;  // tick every 30 seconds
```

## PERF-SC-04 Fix

Previously, a periodic task with a missing `next_run` value in the store would fire immediately on the next tick regardless of its cron schedule. The fix: when `next_run` is `NULL`, the scheduler computes and persists the next occurrence from the cron expression and skips the current tick. Tasks now only fire when `next_run <= now`.

## JobStore Schema

```sql
CREATE TABLE IF NOT EXISTS scheduled_jobs (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    name      TEXT NOT NULL UNIQUE,
    cron_expr TEXT NOT NULL DEFAULT '',
    kind      TEXT NOT NULL,
    last_run  TEXT,
    next_run  TEXT,
    status    TEXT NOT NULL DEFAULT 'pending',
    task_mode TEXT NOT NULL DEFAULT 'periodic',
    run_at    TEXT
)
```

`task_mode` is `'periodic'` or `'oneshot'`. `run_at` holds the ISO 8601 UTC timestamp for one-shot tasks. The `init()` method applies `ALTER TABLE` migrations for older schemas that lack `task_mode` and `run_at`.

## Installation

```bash
cargo add zeph-scheduler
```

Enabled via the `scheduler` feature flag on the root `zeph` crate.

> [!IMPORTANT]
> Requires Rust 1.88 or later.

## License

MIT
