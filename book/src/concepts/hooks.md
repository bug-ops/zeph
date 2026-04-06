# Reactive Hooks

Zeph can run shell commands automatically in response to environment changes. Two hook events are supported: working directory changes and file system changes.

## Hook Types

### `cwd_changed`

Fires when the agent's working directory changes — either via the `set_working_directory` tool or an explicit directory change detected after tool execution.

```toml
[[hooks.cwd_changed]]
command = "echo"
args = ["Changed to $ZEPH_NEW_CWD"]

[[hooks.cwd_changed]]
command = "git"
args = ["status", "--short"]
```

Environment variables available to the hook process:

| Variable | Description |
|----------|-------------|
| `ZEPH_OLD_CWD` | Previous working directory |
| `ZEPH_NEW_CWD` | New working directory |

### `file_changed`

Fires when a file under `watch_paths` is modified. Changes are detected via `notify-debouncer-mini` with a 500 ms debounce window — rapid successive modifications produce a single event.

```toml
[hooks.file_changed]
watch_paths = ["src/", "config.toml"]

[[hooks.file_changed.handlers]]
command = "cargo"
args = ["check", "--quiet"]

[[hooks.file_changed.handlers]]
command = "echo"
args = ["File changed: $ZEPH_CHANGED_PATH"]
```

Environment variable available to the hook process:

| Variable | Description |
|----------|-------------|
| `ZEPH_CHANGED_PATH` | Absolute path of the changed file |

## The `set_working_directory` Tool

The `set_working_directory` tool gives the LLM an explicit, persistent way to change the agent's working directory. Unlike `cd` in a `bash` tool call (which is ephemeral and scoped to one subprocess), `set_working_directory` updates the agent's global cwd and triggers any `cwd_changed` hooks.

```text
Use set_working_directory to switch into /path/to/project
```

After the tool executes, subsequent `bash` and file tool calls run relative to the new directory.

## TUI Indicator

When a hook fires, the TUI status bar shows a short spinner message:

- `cwd_changed` → `Working directory changed…`
- `file_changed` → `File changed: <path>…`

The indicator disappears once all hook commands for that event have completed.

## Configuration Reference

```toml
# cwd_changed hooks — run in order when the working directory changes
[[hooks.cwd_changed]]
command = "echo"
args = ["cwd is now $ZEPH_NEW_CWD"]

# file_changed hooks — watch_paths + handler list
[hooks.file_changed]
watch_paths = ["src/", "tests/"]   # relative or absolute paths to watch
debounce_ms = 500                  # debounce window in milliseconds (default: 500)

[[hooks.file_changed.handlers]]
command = "cargo"
args = ["check", "--quiet"]
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `hooks.cwd_changed[].command` | `string` | — | Executable to run |
| `hooks.cwd_changed[].args` | `Vec<String>` | `[]` | Arguments (env vars expanded) |
| `hooks.file_changed.watch_paths` | `Vec<String>` | `[]` | Paths to monitor |
| `hooks.file_changed.debounce_ms` | `u64` | `500` | Debounce window in milliseconds |
| `hooks.file_changed.handlers[].command` | `string` | — | Executable to run |
| `hooks.file_changed.handlers[].args` | `Vec<String>` | `[]` | Arguments (env vars expanded) |

## Reactive Events

Zeph fires reactive events when the environment changes beneath the agent. Events are processed synchronously before the next agent turn, ensuring hooks complete before the LLM sees the updated context.

### `CwdChanged`

Fires after every tool execution turn when `std::env::current_dir()` differs from the directory recorded at the start of the turn. This covers both explicit `set_working_directory` calls and any side effects from shell commands that change the process cwd.

Hook commands receive the old and new paths via environment variables:

| Variable | Description |
|----------|-------------|
| `ZEPH_OLD_CWD` | Working directory before the change |
| `ZEPH_NEW_CWD` | Working directory after the change |

**Use cases:**
- Auto-run `git status` when switching into a different repo
- Reload environment variables (e.g., `.envrc`) when entering a project directory
- Notify external tools (e.g., tmux pane title, status bar) of the active project

```toml
[[hooks.cwd_changed]]
type         = "command"
command      = "git"
args         = ["status", "--short"]
timeout_secs = 10
fail_closed  = false

[[hooks.cwd_changed]]
type         = "command"
command      = "echo"
args         = ["Entered $ZEPH_NEW_CWD"]
timeout_secs = 5
fail_closed  = false
```

### `FileChanged`

Fires when a file under one of the configured `watch_paths` is modified. The watcher uses `notify-debouncer-mini` with a configurable debounce window (default: 500 ms), so rapid successive writes produce a single event.

The changed file path is passed to hook commands via:

| Variable | Description |
|----------|-------------|
| `ZEPH_CHANGED_PATH` | Absolute path of the modified file |

**Use cases:**
- Run `cargo check` on every save during a coding session
- Regenerate documentation when a source file changes
- Invalidate a cache or restart a development server

Configure glob patterns for `watch_paths` and add one or more handler commands:

```toml
[hooks.file_changed]
watch_paths  = ["src/", "tests/", "Cargo.toml"]
debounce_ms  = 300

[[hooks.file_changed.hooks]]
type         = "command"
command      = "cargo"
args         = ["check", "--quiet"]
timeout_secs = 30
fail_closed  = false

[[hooks.file_changed.hooks]]
type         = "command"
command      = "echo"
args         = ["Changed: $ZEPH_CHANGED_PATH"]
timeout_secs = 5
fail_closed  = false
```

`watch_paths` accepts relative paths (resolved from the agent's working directory at startup) or absolute paths. Directories are watched recursively.

### Hook Execution Model

Each hook definition (`HookDef`) carries:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `type` | `string` | — | Always `"command"` |
| `command` | `string` | — | Executable to run (must be on `PATH` or an absolute path) |
| `args` | `Vec<String>` | `[]` | Arguments; `$VAR` references in args are expanded from the hook environment |
| `timeout_secs` | `u64` | `10` | Maximum time to wait for the command to complete |
| `fail_closed` | `bool` | `false` | When `true`, a hook failure blocks the agent turn; when `false`, failures are logged as warnings |

Multiple hooks for the same event are executed in declaration order. If `fail_closed = true` on any hook, a failure in that hook stops execution of subsequent hooks for that event.
