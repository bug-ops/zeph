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
