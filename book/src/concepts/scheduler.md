# Scheduler

The scheduler runs background tasks on a cron schedule or at a specific future time, persisting job state in SQLite so tasks survive restarts. It is an optional, feature-gated component (`--features scheduler`) that integrates with the agent loop through three LLM-callable tools. The scheduler is **enabled by default** when the feature is compiled in.

## Prerequisites

Enable the `scheduler` feature flag before building:

```bash
cargo build --release --features scheduler
```

See [Feature Flags](../reference/feature-flags.md) for the full flag list.

## Task Modes

Every task has one of two execution modes:

| Mode | Struct variant | Trigger |
|------|---------------|---------|
| `Periodic` | `TaskMode::Periodic { schedule }` | Fires repeatedly on a 5 or 6-field cron expression |
| `OneShot` | `TaskMode::OneShot { run_at }` | Fires once at the given UTC timestamp, then is removed |

The scheduler ticks every 60 seconds by default. `run_with_interval(secs)` accepts a custom interval (minimum 1 second).

## Task Kinds

The `kind` field identifies what handler executes when the task fires:

| Kind string | `TaskKind` variant | Default handler |
|-------------|-------------------|----------------|
| `memory_cleanup` | `TaskKind::MemoryCleanup` | Prune old memory entries |
| `skill_refresh` | `TaskKind::SkillRefresh` | Reload skills from disk |
| `health_check` | `TaskKind::HealthCheck` | Internal liveness probe |
| `update_check` | `TaskKind::UpdateCheck` | Check GitHub Releases for a new version |
| `experiment` | `TaskKind::Experiment` | Run an automatic experiment session (requires `experiments` feature) |
| any other string | `TaskKind::Custom(s)` | `CustomTaskHandler` or agent-loop injection |

Unknown kinds are accepted at runtime and stored as `Custom`. If no handler is registered for a kind when the task fires, the task is skipped with a `debug`-level log entry.

## Cron Expression Format

The scheduler accepts both standard 5-field cron expressions (`min hour day month weekday`) and
6-field expressions with an explicit seconds field (`sec min hour day month weekday`). When a
5-field expression is provided, seconds default to `0`.

```
0 3 * * *         # daily at 03:00 UTC (5-field, standard)
0 2 * * SUN       # Sundays at 02:00 UTC (5-field, standard)
*/15 * * * *      # every 15 minutes (5-field, standard)
0 0 3 * * *       # daily at 03:00 UTC (6-field, with seconds)
0 0 2 * * SUN     # Sundays at 02:00 UTC (6-field, with seconds)
0 */15 * * * *    # every 15 minutes (6-field, with seconds)
* * * * * *       # every second (6-field, testing only)
```

Expressions are parsed by the [`cron`](https://docs.rs/cron) crate. An invalid expression is rejected immediately with `SchedulerError::InvalidCron`.

## LLM-Callable Tools

When the `scheduler` feature is enabled, `SchedulerExecutor` registers three tools with the agent so the LLM can manage tasks in natural language.

### `schedule_periodic`

Schedule a recurring task using a cron expression.

```json
{
  "name": "daily-cleanup",
  "cron": "0 0 3 * * *",
  "kind": "memory_cleanup",
  "config": {}
}
```

| Parameter | Type | Constraints |
|-----------|------|-------------|
| `name` | string | Max 128 characters; unique — scheduling with an existing name **updates** the task |
| `cron` | string | Max 64 characters; must be a valid 5 or 6-field cron expression |
| `kind` | string | Max 64 characters; see Task Kinds above |
| `config` | JSON object | Optional. Passed verbatim to the handler as `serde_json::Value` |

Returns a summary string indicating whether the task was created or updated, and its next scheduled run time.

### `schedule_deferred`

Schedule a one-shot task to fire at a specific future time.

```json
{
  "name": "follow-up",
  "run_at": "2026-03-10T18:00:00Z",
  "kind": "custom",
  "task": "Check if PR #1130 was merged and notify the team"
}
```

| Parameter | Type | Constraints |
|-----------|------|-------------|
| `name` | string | Max 128 characters; unique |
| `run_at` | string | Future time in any supported format (see below) |
| `kind` | string | Max 64 characters |
| `task` | string | Optional. Injected as `Execute the following scheduled task now: <task>` into the agent turn when the task fires (for `custom` kind) |

### `run_at` formats

`run_at` accepts any of the following (must resolve to a future time):

| Format | Example |
|--------|---------|
| ISO 8601 UTC | `2026-03-03T18:00:00Z` |
| ISO 8601 naive (treated as UTC) | `2026-03-03T18:00:00` |
| Relative shorthand | `+2m`, `+1h`, `+30s`, `+1d`, `+1h30m` |
| Natural language | `in 5 minutes`, `in 2 hours`, `today 14:00`, `tomorrow 09:30` |

### `task` field patterns

The `task` string determines how the agent behaves when the task fires. Two patterns:

**Reminder for the user** — the agent notifies the user without acting:

```json
{ "task": "Remind the user to call home" }
{ "task": "Remind the user: standup in 5 minutes" }
```

**Action for the agent** — the agent executes the instruction autonomously:

```json
{ "task": "Check if PR #42 was merged and notify the user" }
{ "task": "Generate an end-of-day summary and send it" }
```

The `task` field is sanitized before injection: control characters below U+0020 (except `\n` and `\t`) are stripped, and the string is truncated to 512 Unicode code points.

### `list_tasks`

List all currently scheduled tasks with their kind, mode, and next run time.

```json
{}
```

Returns a formatted table with columns: NAME, KIND, MODE, and NEXT RUN. No parameters required. Also available as the `/scheduler list` slash command in the CLI and TUI, or as `/scheduler` with no subcommand.

### `cancel_task`

Cancel a scheduled task by name. Works for both periodic and one-shot tasks.

```json
{
  "name": "daily-cleanup"
}
```

Returns `"Cancelled task '<name>'"` if the task existed, or `"Task '<name>' not found"` otherwise.

## Static Task Registration

For tasks that must always be present at startup, register them programmatically before calling `scheduler.init()`:

```rust
# use zeph_scheduler::{JobStore, Scheduler, ScheduledTask, TaskKind};
# use tokio::sync::watch;
# async fn example(store: JobStore) -> anyhow::Result<()> {
let (_shutdown_tx, shutdown_rx) = watch::channel(false);
let (mut scheduler, _msg_tx) = Scheduler::new(store, shutdown_rx);

let task = ScheduledTask::new(
    "daily-cleanup",
    "0 0 3 * * *",
    TaskKind::MemoryCleanup,
    serde_json::Value::Null,
)?;
scheduler.add_task(task);

scheduler.init().await?;
tokio::spawn(async move { scheduler.run().await });
# Ok(())
# }
```

`init()` persists each task to the `scheduled_jobs` SQLite table and computes the initial `next_run` timestamp. Subsequent restarts reuse the persisted `next_run` — tasks do not fire spuriously on boot.

## Custom Task Handlers

Implement the `TaskHandler` trait to execute arbitrary async logic when a task fires:

```rust
# use std::pin::Pin;
# use std::future::Future;
# use zeph_scheduler::{SchedulerError, TaskHandler};
struct MyHandler;

impl TaskHandler for MyHandler {
    fn execute(
        &self,
        config: &serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), SchedulerError>> + Send + '_>> {
        Box::pin(async move {
            // perform work using config
            Ok(())
        })
    }
}
```

Register the handler before starting the loop:

```rust
# use zeph_scheduler::{Scheduler, TaskKind};
# fn example(scheduler: &mut Scheduler) {
scheduler.register_handler(&TaskKind::HealthCheck, Box::new(MyHandler));
# }
```

## Custom One-Shot Tasks and Agent Injection

For `custom` kind one-shot tasks scheduled via the LLM, the scheduler injects the sanitized `task` string directly into the agent loop at fire time. This requires attaching a `custom_task_tx` sender:

```rust
# use tokio::sync::mpsc;
# use zeph_scheduler::Scheduler;
# fn example(scheduler: Scheduler, agent_tx: mpsc::Sender<String>) -> Scheduler {
let scheduler = scheduler.with_custom_task_sender(agent_tx);
# scheduler
# }
```

When the task fires and no handler is registered for `Custom(_)`, the scheduler calls `try_send` on this channel, delivering the prompt as a new agent conversation turn.

## Sanitization

The `sanitize_task_prompt` function protects the agent loop from malformed input in the `task` field:

- Strips all Unicode control characters below U+0020, except `\n` (U+000A) and `\t` (U+0009)
- Truncates to 512 Unicode code points (not bytes), preserving multibyte safety

## Configuration

Add a `[scheduler]` section to `config.toml` to declare static tasks:

```toml
[scheduler]
enabled = true
tick_secs = 60      # scheduler poll interval in seconds (minimum: 1)
max_tasks = 100     # maximum number of concurrent tasks

[[scheduler.tasks]]
name = "daily-cleanup"
cron = "0 0 3 * * *"
kind = "memory_cleanup"

[[scheduler.tasks]]
name = "weekly-skill-refresh"
cron = "0 0 2 * * SUN"
kind = "skill_refresh"
```

## Persistence and Recovery

Job metadata is stored in the `scheduled_jobs` SQLite table (same database as memory). Each row tracks:

- `name` — unique task identifier
- `cron_expr` — cron string for periodic tasks (empty for one-shot)
- `task_mode` — `"periodic"` or `"oneshot"`
- `kind` — task kind string
- `next_run` — RFC 3339 UTC timestamp of the next scheduled firing
- `last_run` — RFC 3339 UTC timestamp of the last successful execution
- `run_at` — target timestamp for one-shot tasks
- `done` — boolean; set to true after a one-shot completes

After a process restart, `next_run` is read from the database. If `next_run` is `NULL` for a periodic task (e.g., first boot after an upgrade), the scheduler computes and persists the next occurrence on the following tick rather than firing immediately.

## Shutdown

The scheduler listens on a `watch::Receiver<bool>` shutdown signal and exits the loop cleanly when `true` is sent:

```rust
# use tokio::sync::watch;
let (shutdown_tx, shutdown_rx) = watch::channel(false);
// ... build and start scheduler ...
let _ = shutdown_tx.send(true); // signal shutdown
```

## Listing Tasks

Use any of the following to view all scheduled tasks:

- **CLI / slash command**: `/scheduler list` (or `/scheduler` with no subcommand) — prints a table with NAME, KIND, MODE, and NEXT RUN columns.
- **LLM tool**: ask the agent "list my scheduled tasks" — the `list_tasks` tool is called automatically.
- **TUI command palette**: open the palette with `:`, type `scheduler`, and select `scheduler:list`.

## TUI Integration

When both `tui` and `scheduler` features are enabled, the command palette includes a `scheduler:list` entry. Open the palette with `:` in normal mode, type `scheduler`, and select the entry to display all active tasks as a table with columns NAME, KIND, MODE, and NEXT RUN.

The task list is refreshed from SQLite every 30 seconds in the background. Background task execution is indicated by the system status spinner in the TUI status bar.

## Related

- [Experiments](experiments.md) — autonomous self-tuning engine with scheduled runs via `[experiments.schedule]`
- [Daemon Mode](../advanced/daemon.md) — running the scheduler alongside the gateway and A2A server
- [Feature Flags](../reference/feature-flags.md) — enabling the `scheduler` feature
- [Tools](tools.md) — how `SchedulerExecutor` integrates with the tool system
