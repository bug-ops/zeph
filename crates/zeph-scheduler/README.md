# zeph-scheduler

Cron-based periodic task scheduler with SQLite persistence.

## Overview

Runs recurring tasks on cron schedules, persisting job state and last-run timestamps in SQLite. Ships with built-in tasks for memory cleanup, skill refresh, health checks, and automatic update detection. Feature-gated behind `scheduler`.

## Key Modules

- **scheduler** — `Scheduler` event loop managing job evaluation and dispatch
- **store** — `JobStore` for SQLite-backed job persistence
- **task** — `ScheduledTask`, `TaskHandler`, `TaskKind` defining task types and execution
- **update_check** — `UpdateCheckHandler` for GitHub releases version check
- **error** — `SchedulerError` error types

## Built-in Tasks

| Kind | String key | Description |
|------|-----------|-------------|
| `TaskKind::MemoryCleanup` | `memory_cleanup` | Prune expired memory entries |
| `TaskKind::SkillRefresh` | `skill_refresh` | Hot-reload changed skill files |
| `TaskKind::HealthCheck` | `health_check` | Periodic self-diagnostics |
| `TaskKind::UpdateCheck` | `update_check` | Check GitHub releases for a newer version |

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
New version available: v0.12.0 (current: v0.11.3).
Update: https://github.com/bug-ops/zeph/releases/tag/v0.12.0
```

Behaviour on error (network failure, non-2xx response, oversized body, parse error, invalid semver) — logs a `warn` message and returns `Ok(())`. The check is best-effort and never crashes the agent.

## Usage

```toml
# Cargo.toml (workspace root)
zeph-scheduler = { path = "crates/zeph-scheduler" }
```

Enabled via the `scheduler` feature flag on the root `zeph` crate.

## License

MIT
