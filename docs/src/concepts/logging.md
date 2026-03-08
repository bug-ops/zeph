# Logging

Zeph supports persistent file-based logging alongside the standard stderr output. File logging uses `tracing-appender` for non-blocking writes with automatic log rotation, keeping your agent sessions observable without impacting performance.

## How it works

Zeph initialises two independent tracing layers at startup:

| Layer | Controlled by | Default level |
|-------|--------------|---------------|
| **stderr** | `RUST_LOG` env var | `info` |
| **file** | `[logging] level` config field | `info` |

The two layers are completely independent. `RUST_LOG` governs what appears on stderr (or your terminal), while the `[logging]` config section governs what is written to the log file. You can set `RUST_LOG=warn` for quiet terminal output while keeping `level = "debug"` in the config to capture detailed file logs.

## Configuration

```toml
[logging]
file = ".zeph/logs/zeph.log"  # Path to the log file (default; empty string disables)
level = "info"                 # File log level: trace, debug, info, warn, error
rotation = "daily"             # Rotation strategy: daily, hourly, or never
max_files = 7                  # Rotated log files to retain (default: 7)
```

### Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `file` | string | `.zeph/logs/zeph.log` | Log file path. Set to `""` to disable file logging entirely |
| `level` | string | `info` | Minimum severity written to the file. Accepts any `tracing` directive (`trace`, `debug`, `info`, `warn`, `error`, or module-level filters like `zeph_core=debug`) |
| `rotation` | string | `daily` | How often to rotate: `daily`, `hourly`, or `never` |
| `max_files` | integer | `7` | Number of rotated log files kept before the oldest is removed |

The log directory is created automatically if it does not exist.

## CLI override

Use `--log-file` to override the file path for a single session:

```bash
# Log to a custom path
zeph --log-file /tmp/debug-session.log

# Disable file logging for this run
zeph --log-file ""
```

Priority: `--log-file` > `ZEPH_LOG_FILE` env var > `[logging] file` config value.

## Environment variables

| Variable | Description |
|----------|-------------|
| `ZEPH_LOG_FILE` | Override `logging.file` |
| `ZEPH_LOG_LEVEL` | Override `logging.level` |

## Interactive command

During a session, type `/log` to display the current logging configuration and the last 20 lines of the log file:

```
> /log
Log file:  .zeph/logs/zeph.log
Level:     info
Rotation:  daily
Max files: 7

Recent entries:
2026-03-09T10:15:32.000Z  INFO zeph_core::agent: turn completed tokens=1523
...
```

## Init wizard

The `zeph init` wizard includes a logging step where you can configure:

1. Log file path (or leave empty to disable)
2. File log level
3. Log rotation strategy

## RUST_LOG vs file level

| Scenario | `RUST_LOG` | `[logging] level` | Result |
|----------|-----------|-------------------|--------|
| Quiet terminal, verbose file | `warn` | `debug` | Terminal shows warnings+errors; file captures everything from debug up |
| Debug both | `debug` | `debug` | Both sinks receive debug-level output |
| File only | _(unset, defaults to info)_ | `trace` | Terminal at info; file captures all trace events |
| No file logging | any | _(file = "")_ | Only stderr output; no file layer created |

> [!TIP]
> For deep debugging sessions, combine `RUST_LOG=debug` with `level = "debug"` in the config to get full output in both sinks. Redirect stderr if needed: `RUST_LOG=debug zeph 2>/dev/null`.
