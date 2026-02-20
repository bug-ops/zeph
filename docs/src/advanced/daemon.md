# Daemon and Scheduler

Run Zeph as a long-running process with component supervision and cron-based periodic tasks.

## Headless Daemon Mode

The `--daemon` flag starts Zeph as a headless background agent with full capabilities (LLM, tools, memory, MCP) exposed via an A2A JSON-RPC endpoint. Requires both `daemon` and `a2a` features.

```bash
cargo build --release --features daemon,a2a
zeph --daemon
```

The daemon bootstraps a complete agent using a `LoopbackChannel` for internal I/O, starts the A2A server, and runs under `DaemonSupervisor` with PID file lifecycle and graceful Ctrl-C shutdown. Connect a TUI client with `--connect` for real-time streaming interaction.

See the [Daemon Mode guide](../guides/daemon-mode.md) for configuration, usage, and architecture details.

## Daemon Supervisor

The daemon manages component lifecycles (gateway, scheduler, A2A server), monitors for unexpected exits, and tracks restart counts.

### Feature Flag

```bash
cargo build --release --features daemon
```

### Configuration

```toml
[daemon]
enabled = true
pid_file = "~/.zeph/zeph.pid"
health_interval_secs = 30
max_restart_backoff_secs = 60
```

### Component Lifecycle

Each registered component is tracked with a status (`Running`, `Failed(reason)`, or `Stopped`) and a restart counter. The supervisor polls all components at `health_interval_secs` intervals.

### PID File

Written on startup for instance detection and stop signals. Tilde (`~`) expands to `$HOME`. Parent directory is created automatically.

## Cron Scheduler

Run periodic tasks on cron schedules with SQLite-backed persistence.

### Feature Flag

```bash
cargo build --release --features scheduler
```

### Configuration

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

Cron expressions use 6 fields: `sec min hour day month weekday`. Standard features supported: ranges (`1-5`), lists (`1,3,5`), steps (`*/5`), wildcards (`*`).

### Built-in Tasks

| Kind | Description |
|------|-------------|
| `memory_cleanup` | Remove old conversation history entries |
| `skill_refresh` | Re-scan skill directories for changes |
| `health_check` | Internal health verification |
| `update_check` | Query GitHub Releases API for newer versions |

### Update Check

Controlled by `auto_update_check` in `[agent]` (default: true):

- **With scheduler**: runs daily at 09:00 UTC via cron task
- **Without scheduler**: single one-shot check at startup

### Custom Tasks

Implement the `TaskHandler` trait:

```rust
pub trait TaskHandler: Send + Sync {
    fn execute(
        &self,
        config: &serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), SchedulerError>> + Send + '_>>;
}
```

### Persistence

Job metadata is stored in a `scheduled_jobs` SQLite table. The scheduler ticks every 60 seconds and checks whether each task is due based on `last_run` and the cron expression.

## Shutdown

Both daemon and scheduler listen on the global shutdown signal and exit gracefully.
