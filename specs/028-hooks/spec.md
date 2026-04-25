---
aliases:
  - Reactive Hooks
  - File Hooks
  - Directory Hooks
tags:
  - sdd
  - spec
  - core
  - hooks
  - runtime
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[027-runtime-layer/spec]]"
  - "[[018-scheduler/spec]]"
---

# Spec: Reactive Hooks

> **Crate**: `zeph-core`

## Sources

### Internal

| File | Contents |
|---|---|
| `crates/zeph-core/src/hooks/mod.rs` | `HookRunner`, hook dispatch, env var injection |
| `crates/zeph-core/src/hooks/file_watcher.rs` | `FileChangeWatcher` via `notify-debouncer-mini` |
| `crates/zeph-core/src/tools/native/cwd.rs` | `set_working_directory` native tool |
| `crates/zeph-core/src/config/types/hooks.rs` | `HooksConfig`, `HookEntry` |

---

## Overview

Reactive hooks allow operators to run shell commands or invoke MCP tools in response to
agent lifecycle events. Five event types are supported:

| Event | Trigger |
|---|---|
| `cwd_changed` | Agent working directory changes (via `set_working_directory` tool) |
| `file_changed` | A watched file or directory subtree is modified on disk |
| `permission_denied` | A tool execution is short-circuited by a `RuntimeLayer::before_tool` check |
| `turn_complete` | An agent turn completes (after all tool calls and LLM response) |
| `post_tool_use` | After any tool invocation completes (carries `ZEPH_TOOL_DURATION_MS`) |

Hooks are defined as arrays under `[hooks]` in `config.toml`. Each entry specifies
a hook action and optional filters. Multiple hooks per event are supported and run
sequentially.

> [!note] Action types
> Two action types are available: `type = "command"` (default) runs a shell command;
> `type = "mcp_tool"` invokes an MCP server tool directly without spawning a subprocess.
> See the **Action Types** section below.

---

## Config

```toml
[[hooks.cwd_changed]]
type = "command"
command = "echo 'cwd changed to $ZEPH_NEW_CWD'"

[[hooks.file_changed]]
type = "command"
command = "cargo check"
glob = "src/**/*.rs"    # optional; only fire when changed path matches this glob

[[hooks.permission_denied]]
type = "command"
command = "echo 'tool $ZEPH_DENIED_TOOL blocked: $ZEPH_DENY_REASON'"

[[hooks.turn_complete]]
type = "command"
command = "osascript -e 'display notification \"$ZEPH_TURN_PREVIEW\" with title \"Zeph\"'"

[[hooks.post_tool_use]]
type = "command"
command = "echo 'tool took ${ZEPH_TOOL_DURATION_MS}ms'"
```

Multiple entries of the same type are permitted.

## Action Types

### `type = "command"` (default)

Runs a shell command via the same shell executor infrastructure as the `bash` tool.

### `type = "mcp_tool"`

Invokes an MCP server tool directly without spawning a subprocess (#3293). Requires
the MCP manager to be active; fails according to `fail_closed` if unavailable.

```toml
[[hooks.cwd_changed]]
type = "mcp_tool"
server = "my-server"
tool = "notify"
args = { message = "cwd changed" }    # optional static args
```

> [!warning]
> `HookDef.hook_type + command` fields replaced by `HookDef.action: HookAction`
> (serde-flattened) as a breaking config change (#3293). Existing TOML with
> `type = "command"` deserializes correctly — no manual migration needed.

---

## Environment Variables

Hooks receive context via environment variables injected into the shell command:

| Variable | Event | Value |
|---|---|---|
| `ZEPH_OLD_CWD` | `cwd_changed` | Previous working directory (absolute path) |
| `ZEPH_NEW_CWD` | `cwd_changed` | New working directory (absolute path) |
| `ZEPH_CHANGED_PATH` | `file_changed` | Absolute path of the changed file or directory |
| `ZEPH_DENIED_TOOL` | `permission_denied` | Name of the tool that was blocked |
| `ZEPH_DENY_REASON` | `permission_denied` | Human-readable reason from `LayerDenial.reason` |
| `ZEPH_TURN_DURATION_MS` | `turn_complete` | Wall-clock duration of the turn in milliseconds |
| `ZEPH_TURN_STATUS` | `turn_complete` | `"success"` or `"error"` |
| `ZEPH_TURN_PREVIEW` | `turn_complete` | Redacted short preview of the LLM response |
| `ZEPH_TURN_LLM_REQUESTS` | `turn_complete` | Number of LLM requests in the turn |
| `ZEPH_TOOL_DURATION_MS` | `post_tool_use` | Wall-clock duration of the tool call in milliseconds |

> [!note] `turn_complete` gate
> When a `[notifications]` notifier is configured, `turn_complete` shares its `should_fire`
> gate (respects `only_on_error`, `min_turn_duration_ms`, etc.). When no notifier is present,
> `turn_complete` fires on every turn unconditionally.

---

## `set_working_directory` Tool

`set_working_directory` is a native tool (always available, no feature flag) that
changes the agent's working directory and fires all `cwd_changed` hooks.

```json
{
  "tool": "set_working_directory",
  "params": { "path": "/absolute/or/relative/path" }
}
```

- Relative paths are resolved relative to the current working directory
- The new directory must exist; non-existent paths produce `ToolError::PermanentFailure`
- After a successful directory change, all `[[hooks.cwd_changed]]` hooks are invoked

---

## FileChangeWatcher

`FileChangeWatcher` uses the `notify-debouncer-mini` crate to watch paths for
filesystem changes. It runs in a background tokio task and fires `file_changed`
hooks via the hook runner.

### Debouncing

Events are debounced with a fixed delay (default 200 ms) to coalesce rapid
file-system events (e.g., editor save sequences that touch a file multiple times).
Only the last event per path within the debounce window is delivered.

### Glob Filtering

When a `[[hooks.file_changed]]` entry has a `glob` field, only change events
whose `ZEPH_CHANGED_PATH` matches the glob are forwarded to that hook's command.
Hooks without a `glob` field fire on every change event.

---

## Hook Execution

Hooks are run as shell commands via the same shell executor infrastructure as the
`bash` tool. Hook commands run with the agent's current working directory as the
working directory.

- Hook stdout and stderr are logged at `DEBUG` level — not injected into agent context
- Hook exit code is logged; non-zero exit code emits a `WARN`
- Hook execution is non-blocking from the agent's perspective: hooks run in a
  background task spawned by the hook runner
- Hook failures are non-fatal — a failing hook does not abort the event or the turn

---

## Key Invariants

- Hook commands execute with the blocked-command list applied — dangerous shell patterns are prevented
- `ZEPH_OLD_CWD`, `ZEPH_NEW_CWD`, `ZEPH_CHANGED_PATH` are always absolute, canonicalized paths
- Hooks do NOT receive agent conversation context — they are environment-aware but not LLM-aware
- `set_working_directory` must fire `cwd_changed` hooks synchronously before returning `ToolOutput` — the new cwd must be committed first
- `FileChangeWatcher` debounce is mandatory — raw filesystem events must never bypass it
- File change watcher is skipped in `--bare` mode — `with_hooks_config` is guarded by `!exec_mode.bare` in `runner.rs` (#3362)
- Hook execution is never on the agent hot path — always background task
- `permission_denied` hook fires when `RuntimeLayer::before_tool` short-circuits execution; `LayerDenial.reason` is propagated to `ZEPH_DENY_REASON` (#3310)
- `turn_complete` is added to `HooksConfig` and `HooksConfig::is_empty()` check (#3327)
- `type = "mcp_tool"` action requires MCP manager active; must fail gracefully per `fail_closed` setting when unavailable (#3293)
- NEVER inject hook stdout into the agent's conversation context
- NEVER run hooks with elevated privileges — they inherit the agent process permissions only
- If `[hooks]` section is absent from config, all hook lists are empty and no hooks fire — zero-cost when unused

---

## Agent Boundaries

### Always (without asking)
- Apply the shell blocked-command list to hook commands
- Log hook stdout/stderr at DEBUG, non-zero exit at WARN
- Canonicalize paths before setting `ZEPH_*` env vars

### Ask First
- Adding new hook event types (requires new `FileChanged` / `CwdChanged` variant)
- Changing debounce interval (affects reactivity vs. noise trade-off)

### Never
- Inject hook output into LLM context
- Block the agent turn on hook execution
- Skip the shell blocklist for hook commands
