# zeph-scheduler

Cron-based periodic task scheduler with SQLite persistence.

## Overview

Runs recurring tasks on cron schedules, persisting job state and last-run timestamps in SQLite. Ships with built-in tasks for memory cleanup, skill refresh, and health checks. Feature-gated behind `scheduler`.

## Key Modules

- **scheduler** — `Scheduler` event loop managing job evaluation and dispatch
- **store** — `JobStore` for SQLite-backed job persistence
- **task** — `ScheduledTask`, `TaskHandler`, `TaskKind` defining task types and execution
- **error** — `SchedulerError` error types

## Built-in Tasks

- `memory_cleanup` — prune expired memory entries
- `skill_refresh` — hot-reload changed skill files
- `health_check` — periodic self-diagnostics

## Usage

```toml
# Cargo.toml (workspace root)
zeph-scheduler = { path = "crates/zeph-scheduler" }
```

Enabled via the `scheduler` feature flag on the root `zeph` crate.

## License

MIT
