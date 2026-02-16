# Daemon Supervisor

The daemon supervisor manages component lifecycles within a long-running Zeph process. It monitors registered components (gateway, scheduler, A2A server) for unexpected exits and tracks restart counts.

## Feature Flag

Enable with `--features daemon` at build time:

```bash
cargo build --release --features daemon
```

## Configuration

Add the `[daemon]` section to `config/default.toml`:

```toml
[daemon]
enabled = true
pid_file = "~/.zeph/zeph.pid"
health_interval_secs = 30
max_restart_backoff_secs = 60
```

### PID File

The daemon writes its process ID to `pid_file` on startup. This file is used to detect running instances and to send stop signals. Tilde (`~`) expands to `$HOME`. The parent directory is created automatically if it does not exist.

## Component Lifecycle

Each registered component is wrapped in a `ComponentHandle` that tracks:

- **name** -- human-readable identifier (e.g., `"gateway"`, `"scheduler"`)
- **status** -- `Running`, `Failed(reason)`, or `Stopped`
- **restart_count** -- number of unexpected exits detected

The supervisor polls all components at `health_interval_secs` intervals. When a running component's task handle reports completion (unexpected exit), the supervisor marks it as `Failed` and increments its restart counter.

## Shutdown

The supervisor listens on the global shutdown signal (`watch::Receiver<bool>`). When the signal fires, the health loop exits and all component handles are dropped.

## PID File Utilities

The `daemon` module provides three standalone functions for PID file management:

| Function | Description |
|----------|-------------|
| `write_pid_file(path)` | Write current process ID to file |
| `read_pid_file(path)` | Read PID from file |
| `remove_pid_file(path)` | Remove PID file (no-op if missing) |
