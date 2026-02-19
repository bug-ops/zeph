# Cron Scheduler

The scheduler runs periodic tasks on cron schedules with SQLite-backed persistence. It tracks last execution times to avoid duplicate runs and supports built-in and custom task kinds.

## Feature Flag

Enable with `--features scheduler` at build time:

```bash
cargo build --release --features scheduler
```

## Configuration

Define tasks in the `[scheduler]` section of `config/default.toml`:

```toml
[scheduler]
enabled = true

[[scheduler.tasks]]
name = "memory_cleanup"
cron = "0 0 0 * * *"          # daily at midnight
kind = "memory_cleanup"
config = { max_age_days = 90 }

[[scheduler.tasks]]
name = "health_check"
cron = "0 */5 * * * *"        # every 5 minutes
kind = "health_check"
```

### Cron Expression Format

The scheduler uses 6-field cron expressions (seconds included):

```
sec  min  hour  day  month  weekday
 0    0    0     *    *      *
```

Standard cron features are supported: ranges (`1-5`), lists (`1,3,5`), steps (`*/5`), and wildcards (`*`).

## Built-in Task Kinds

| Kind | Description |
|------|-------------|
| `memory_cleanup` | Remove old conversation history entries |
| `skill_refresh` | Re-scan skill directories for changes |
| `health_check` | Internal health verification |
| `update_check` | Query GitHub Releases API for a newer version |

Custom kinds are also supported. Register a handler implementing the `TaskHandler` trait for any custom `kind` string.

## Update Check

The `update_check` task uses `UpdateCheckHandler` to query the GitHub Releases API and compare the running version against the latest release. When a newer version is detected, a notification message is emitted to the agent channel.

The update check is controlled by `auto_update_check` in `[agent]` (default: `true`). It is independent of the scheduler feature flag:

- **With `scheduler` feature enabled**: the check runs daily at 09:00 UTC via a cron task (`0 0 9 * * *`).
- **Without `scheduler` feature**: a single one-shot check is performed at startup.

To add the update check to the scheduler task list explicitly:

```toml
[agent]
auto_update_check = true  # default; set to false to disable entirely

[[scheduler.tasks]]
name = "update_check"
cron = "0 0 9 * * *"      # daily at 09:00 UTC
kind = "update_check"
```

The handler uses a 10-second HTTP timeout and caps the response body at 64 KB. Network errors and non-2xx responses are logged as warnings and treated as no-ops, so a failed check never interrupts normal agent operation.

## TaskHandler Trait

Implement `TaskHandler` to define custom task logic:

```rust
pub trait TaskHandler: Send + Sync {
    fn execute(
        &self,
        config: &serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), SchedulerError>> + Send + '_>>;
}
```

The `config` parameter receives the `config` value from the task definition in TOML.

## Persistence

The scheduler stores job metadata in a `scheduled_jobs` SQLite table:

| Column | Type | Description |
|--------|------|-------------|
| `name` | TEXT | Unique task identifier |
| `cron_expr` | TEXT | Cron schedule expression |
| `kind` | TEXT | Task kind string |
| `last_run` | TEXT | ISO 8601 timestamp of last execution |
| `status` | TEXT | Current status (`pending`, `completed`) |

On startup, the scheduler upserts all configured tasks into the table. Each tick (every 60 seconds), it checks whether each task is due based on `last_run` and the cron expression.

## Shutdown

The scheduler listens on the global shutdown signal and exits its tick loop gracefully.
