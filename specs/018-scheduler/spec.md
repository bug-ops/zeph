# Spec: Scheduler

## Sources

### Internal
| File | Contents |
|---|---|
| `crates/zeph-scheduler/src/scheduler.rs` | `Scheduler`, tick loop, `drain_channel`, `tick` |
| `crates/zeph-scheduler/src/task.rs` | `ScheduledTask`, `TaskMode`, `TaskKind`, `TaskDescriptor` |
| `crates/zeph-scheduler/src/store.rs` | SQLite persistence, `JobStore` |
| `crates/zeph-scheduler/src/handlers.rs` | Handler registration and execution |
| `crates/zeph-scheduler/src/sanitize.rs` | Input sanitization for natural language tasks |

---

`crates/zeph-scheduler/` (feature: `scheduler`) — cron-based periodic task scheduler with SQLite persistence.

## Data Model

```
SQLite: scheduled_jobs
├── name: String (unique)
├── schedule: String           — cron expression
├── kind: TaskKind             — HealthCheck | Update | Custom(String)
├── mode: TaskMode             — Periodic { schedule: CronSchedule } | OneShot { run_at }
├── config: serde_json::Value  — handler-specific config
├── next_run: Option<DateTime<Utc>> (RFC3339)
├── last_run: Option<DateTime<Utc>>
└── status: String
```

## Task Lifecycle

```
Created → Init (compute next_run) → Pending → Running → Completed (repeating)
                                             → Failed (retry next tick)
OneShot: Completed → removed from self.tasks
```

## Init (CRITICAL)

`scheduler.init()` must compute and persist `next_run` for **all** periodic tasks before the first tick:

```
schedule.after(now).next()  →  Some(next_run): persist to DB
                             →  None: log WARN, skip task
```

**PERF-SC-04 is FIXED** — the bug where missing `next_run` caused tasks to fire on every tick is resolved:
- Fix: when `next_run` is `None` in DB, compute next occurrence, persist, **do not fire**
- Regression test: `tick_does_not_fire_without_next_run()` protects against this

## Tick Loop

- Default interval: 60s (configurable via `run_with_interval(N)`)
- Each tick:
  1. `drain_channel()` — consume pending `Add`/`Cancel` messages from mpsc
  2. For each task, check `should_run`:
     - **Periodic**: fetch `next_run` from DB, compare `<= now`
     - **OneShot**: compare `run_at <= now`
  3. If `should_run`: execute handler (sequential per tick — no concurrent handler calls)
  4. On success: `record_run()` → update `last_run`, compute `next_run = schedule.after(now).next()`, persist
  5. On failure: log WARN, skip DB update, task stays in queue (retried next tick)
  6. OneShot after execution: `mark_done()`, remove from `self.tasks`

## Runtime Registration

```
SchedulerMessage { Add(TaskDescriptor) | Cancel(String) }
```

- Send to `msg_tx` (mpsc); next `drain_channel()` picks up
- `register_descriptor()`: check `max_tasks` capacity (upsert of existing name bypasses limit), upsert to DB, add to `self.tasks`

## `scheduler` Native Tool

Natural language registration:
```
"remind me every Monday at 9am to check deployments"
→ LLM parses → cron expression → Task { name, schedule, command }
```

- Task fires by injecting into agent `message_queue` — never calls agent methods directly
- Task persists across restarts (SQLite)

## Scheduler CLI Subcommand
> **Status**: Implemented. Closes #2701. Feature-gated under `scheduler`.

New `zeph schedule` subcommand with four sub-commands:

| Sub-command | Action |
|-------------|--------|
| `zeph schedule list` | Table of all active jobs |
| `zeph schedule add <cron> <prompt>` | INSERT new job (fails on duplicate name) |
| `zeph schedule remove <name>` | Delete job by name |
| `zeph schedule show <name>` | Full details including stored prompt |

`add` auto-generates job names from a blake3 hash of the prompt when `--name` is not given. Returns `SchedulerError::DuplicateJob` (instead of upsert) to prevent accidental overwrites.

Migration `072_scheduler_task_data` adds `task_data TEXT NOT NULL DEFAULT ''` to `scheduled_jobs` in both SQLite and Postgres. `JobStore` gains `insert_job` (INSERT without ON CONFLICT). `ScheduledTaskInfo` includes `task_data`.

### New Error Variant
```rust
SchedulerError::DuplicateJob(String)   // returned by insert_job when name already exists
```

### Key Invariants
- `insert_job` uses `INSERT` without `ON CONFLICT` — duplicate job name is always an error
- `SchedulerError::DuplicateJob` must be surfaced to the user with a clear message
- `task_data` stores the natural-language prompt for jobs created via CLI or the `scheduler` tool

---

## Tick Loop MissedTickBehavior
> **Status**: Implemented. Closes #2737.

Both `Scheduler::run_with_interval` and `Scheduler::run` set `MissedTickBehavior::Skip` on their `tokio::time::Interval`. The default `Burst` mode caused hundreds of back-to-back ticks per second when `tick()` took longer than the interval.

### Key Invariant
- ALWAYS use `MissedTickBehavior::Skip` on scheduler intervals — `Burst` is a correctness hazard for tick-sensitive scheduling logic

---

## Key Invariants

- `init()` must compute and persist `next_run` for all periodic tasks before first tick
- `next_run = None` → compute from schedule, persist, **never fire** — PERF-SC-04 regression must not recur
- `cron.after(now)` uses current time, not stored `next_run`, when computing next occurrence after execution
- Handlers execute **sequentially** per tick — no parallel handler invocation
- OneShot tasks must be removed from `self.tasks` after execution (`mark_done` + `retain`)
- On handler failure: task stays queued — never remove on failure
- `max_tasks` limit only applies to **new** tasks (upsert of existing name is allowed)
- Task fires via `message_queue` injection — no direct agent method calls
