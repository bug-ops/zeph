# Reactive Hooks

Zeph can run shell commands automatically in response to environment changes and tool execution events. Four hook events are supported: working directory changes, file system changes, tool execution before/after.

## Hook Types

### `pre_tool_use` and `post_tool_use`

Fires before and after a tool is executed. Useful for logging, monitoring, security auditing, or modifying the environment before/after tool runs.

**Pre-execution (before tool runs):**

```toml
[[hooks.pre_tool_use]]
tools = "shell|bash|sh"              # Pipe-separated tool name patterns (glob matching)
command = "echo"
args = ["About to run: $ZEPH_TOOL_NAME with args: $ZEPH_TOOL_ARGS_JSON"]
```

**Post-execution (after tool runs):**

```toml
[[hooks.post_tool_use]]
tools = "write_file|edit_file"       # File write tools
command = "git"
args = ["add", "$ZEPH_TOOL_NAME"]
fail_closed = false                  # If true, hook failure aborts the tool chain (default: false)
```

Environment variables available to hook processes:

| Variable | Available in | Description |
|----------|---|-------------|
| `ZEPH_TOOL_NAME` | pre + post | Tool name (e.g., `shell`, `web_scrape`) |
| `ZEPH_TOOL_ARGS_JSON` | pre + post | Tool arguments as JSON (truncated to 64 KiB via UTF-8 boundary) |
| `ZEPH_TOOL_DURATION_MS` | post only | Time taken to execute the tool (milliseconds) |
| `ZEPH_SESSION_ID` | pre + post (main agent only) | Session ID; omitted in subagent hooks |

**Hook firing order:**

Pre-hooks fire **before** utility gate and permission checks. This means observers can see all tool invocations, including those that would be blocked by policies. Post-hooks fire after successful execution.

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
# Pre-tool-use hooks — run before any tool execution
[[hooks.pre_tool_use]]
tools = "shell|bash|sh"           # Tool name pattern (pipe-separated, glob matching)
command = "echo"
args = ["Running: $ZEPH_TOOL_NAME"]
fail_closed = false               # If true, hook failure aborts the tool (default: false)

# Post-tool-use hooks — run after tool execution completes
[[hooks.post_tool_use]]
tools = "write_file"
command = "git"
args = ["add", "$ZEPH_TOOL_NAME"]
fail_closed = false               # If true, hook failure blocks subsequent tools

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
| `hooks.pre_tool_use[].tools` | `string` | — | Pipe-separated tool name patterns to match |
| `hooks.pre_tool_use[].command` | `string` | — | Executable to run |
| `hooks.pre_tool_use[].args` | `Vec<String>` | `[]` | Arguments (env vars expanded) |
| `hooks.pre_tool_use[].fail_closed` | `bool` | false | If true, hook failure aborts the tool chain |
| `hooks.post_tool_use[].tools` | `string` | — | Pipe-separated tool name patterns to match |
| `hooks.post_tool_use[].command` | `string` | — | Executable to run |
| `hooks.post_tool_use[].args` | `Vec<String>` | `[]` | Arguments (env vars expanded) |
| `hooks.post_tool_use[].fail_closed` | `bool` | false | If true, hook failure aborts the tool chain |
| `hooks.cwd_changed[].command` | `string` | — | Executable to run |
| `hooks.cwd_changed[].args` | `Vec<String>` | `[]` | Arguments (env vars expanded) |
| `hooks.file_changed.watch_paths` | `Vec<String>` | `[]` | Paths to monitor |
| `hooks.file_changed.debounce_ms` | `u64` | `500` | Debounce window in milliseconds |
| `hooks.file_changed.handlers[].command` | `string` | — | Executable to run |
| `hooks.file_changed.handlers[].args` | `Vec<String>` | `[]` | Arguments (env vars expanded) |

### Tool Pattern Matching

Tool name patterns support pipe-separated patterns and glob matching:

```toml
# Match exact tool names
tools = "shell"                     # Only the shell tool

# Match multiple tools
tools = "shell|bash|sh"             # Any shell variant

# Glob patterns (glob syntax)
tools = "write_*"                   # write_file, write_dir, etc.

# Combine exact and globs
tools = "shell|*_file"              # shell tool or any *_file tool
```

Patterns are matched case-sensitively. An empty pattern matches no tools.

## Hook Tracing and Instrumentation

All hook execution is instrumented with distributed tracing. Each hook invocation generates:

- `zeph.hooks.cwd_changed` span — execution of a `cwd_changed` hook
- `zeph.hooks.file_changed` span — execution of a `file_changed` hook

Spans include:

| Attribute | Value |
|-----------|-------|
| `hook.command` | Executable name (e.g., `cargo`, `git`) |
| `hook.args` | Full argument list |
| `hook.duration_ms` | Execution wall-clock time |
| `hook.exit_code` | Process exit code (if available) |

Traces are exported to your configured telemetry backend (local Chrome JSON or Jaeger OTLP) and are visible in profiling tools like Perfetto. This allows you to identify slow hooks and optimize them.

## Hook Propagation on Config Reload

When `zeph reload-config` is called (or config changes are hot-reloaded), hooks are immediately re-parsed and re-registered. The TUI and scheduler receive hook update notifications so they can reconfigure watchers without restarting.

For `file_changed` hooks:
1. Old watchers are stopped
2. New watch paths are parsed from the updated config
3. Handlers are registered with the new watcher
4. The next file modification triggers the updated hooks

For `cwd_changed` hooks:
1. The hook list is updated in memory
2. The next working directory change fires the new hooks

This enables configuration updates without restarting the agent process.

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

### `TurnComplete`

Fires after each agent turn completes. This hook does not block the turn — it runs fire-and-forget in the background and allows notification integrations, logging, or external system updates to happen after the agent responds.

Hook commands receive environment variables describing the turn outcome:

| Variable | Description |
|----------|-------------|
| `ZEPH_TURN_DURATION_MS` | Turn latency in milliseconds |
| `ZEPH_TURN_STATUS` | `success`, `error`, or `cancelled` |
| `ZEPH_TURN_PREVIEW` | First 150 chars of redacted agent response |
| `ZEPH_TURN_LLM_REQUESTS` | Number of LLM API calls made this turn |

**Use cases:**
- Send a custom notification via a webhook
- Log turn metrics to an external service
- Sync agent state to an external system after each turn

```toml
[[hooks.turn_complete]]
type         = "command"
command      = "curl"
args         = ["-X", "POST", "http://localhost:9999/webhook", "-d", "status=$ZEPH_TURN_STATUS"]
timeout_secs = 5
fail_closed  = false
```

When a `[notifications]` block is configured, `turn_complete` hooks share the same `should_fire` gate — the hook only runs if notifications are also configured to fire. When `[notifications]` is absent or `enabled = false`, `turn_complete` hooks fire on every turn.

### `PermissionDenied`

Fires when a tool execution is blocked by a `RuntimeLayer::before_tool` permission check. This allows you to log or audit blocked tool calls before they reach the user or external systems.

Hook commands receive:

| Variable | Description |
|----------|-------------|
| `ZEPH_DENIED_TOOL` | Name of the blocked tool |
| `ZEPH_DENY_REASON` | Reason the tool was denied (e.g., `"blocked by before_tool layer"`) |

**Use cases:**
- Log security audit events to a central system
- Alert on suspicious tool invocation patterns
- Track which policies are enforcing restrictions

```toml
[[hooks.permission_denied]]
type         = "command"
command      = "logger"
args         = ["-t", "zeph-security", "Denied tool: $ZEPH_DENIED_TOOL - $ZEPH_DENY_REASON"]
timeout_secs = 5
fail_closed  = false
```

## MCP Tool Hooks

Hooks support direct MCP tool invocation via `type = "mcp_tool"`. When `type = "mcp_tool"`, the hook invokes a tool on a connected MCP server instead of spawning a subprocess.

```toml
[[hooks.cwd_changed]]
type     = "mcp_tool"
server   = "filesystem"        # MCP server id
tool     = "write_file"        # MCP tool name
args     = {"path": "/tmp/log", "contents": "Changed to $ZEPH_NEW_CWD"}
fail_closed = false            # ignored if server unavailable
```

MCP tool hooks require the MCP manager to be active. If the server is unavailable, the hook result depends on `fail_closed`:

- `fail_closed = false` (default): error is logged and the turn continues
- `fail_closed = true`: turn is blocked until the tool succeeds or timeout expires
